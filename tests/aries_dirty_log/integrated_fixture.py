#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse, base64, hashlib, http.client, json, os, pathlib, re, socket, subprocess, time

class Conn(http.client.HTTPConnection):

    def __init__(self, path):
        super().__init__('localhost', timeout=30)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(self.path))

def run(out, mem_mib, binary, kernel, initrd, kernel_source, tap):
    out.mkdir(parents=True)
    sock = out / 'api.sock'
    vs = out / 'vs.sock'

    def call(method, path, body=None, expected=200):
        c = Conn(sock)
        c.request(method, path, json.dumps(body) if body is not None else None, {'Content-Type': 'application/json'})
        r = c.getresponse()
        raw = r.read()
        c.close()
        d = json.loads(raw) if raw else None
        calls.append({'method': method, 'path': path, 'body': body, 'status': r.status, 'response': d})
        if r.status >= 400 and r.status != expected:
            raise RuntimeError((path, r.status, d))
        if expected >= 400:
            assert r.status == expected, (path, r.status, d)
        return d

    def wait_text(pattern, seconds=40):
        end = time.time() + seconds
        while time.time() < end:
            s = (out / 'serial.log').read_text(errors='replace')
            m = re.search(pattern, s)
            if m:
                return m
            if proc.poll() is not None:
                raise RuntimeError('FC exited ' + str(proc.returncode) + ' ' + s[-2000:])
            time.sleep(0.1)
        raise RuntimeError('missing serial ' + pattern + ' ' + s[-2000:])

    def dirty_pages(reply, key):
        pages = set()
        for r in reply['regions']:
            b = base64.b64decode(r[key])
            base = r['guest_base'] // 4096
            for i in range(r['page_count']):
                if b[i // 8] >> i % 8 & 1:
                    pages.add(base + i)
        return pages

    def marker_gpas(reply, marker):
        maps = []
        for line in pathlib.Path(f'/proc/{proc.pid}/maps').read_text().splitlines():
            a, b = (int(x, 16) for x in line.split()[0].split('-'))
            if 'rw' in line.split()[1]:
                maps.append((a, b))
        fd = os.open(f'/proc/{proc.pid}/mem', os.O_RDONLY)
        found = []
        try:
            for r in reply['regions']:
                candidates = [a for a, b in maps if b - a == r['bytes']]
                assert len(candidates) == 1, ('RAM mapping ambiguous', r['guest_base'], r['bytes'], candidates)
                hva = candidates[0]
                base = r['guest_base'] // 4096
                for page in dirty_pages(reply, 'vmm_dirty_b64'):
                    offset = (page - base) * 4096
                    if 0 <= offset < r['bytes'] and marker in os.pread(fd, 4096, hva + offset):
                        found.append(page)
        finally:
            os.close(fd)
        return found
    calls = []
    results = {'kernel_sha256': hashlib.sha256(kernel.read_bytes()).hexdigest(), 'kernel_source': kernel_source, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'host_uname': os.uname().release, 'mem_mib': mem_mib}
    proc = None
    conn = None
    udp = None
    tap_created = False
    try:
        subprocess.run(['ip', 'tuntap', 'add', 'dev', tap, 'mode', 'tap'], check=True)
        tap_created = True
        subprocess.run(['ip', 'addr', 'add', '10.231.218.1/24', 'dev', tap], check=True)
        subprocess.run(['ip', 'link', 'set', tap, 'up'], check=True)
        subprocess.run(['ip', 'neigh', 'replace', '10.231.218.2', 'lladdr', '06:00:ac:10:00:02', 'dev', tap], check=True)
        disk = out / 'block.bin'
        with disk.open('wb') as f:
            f.write(bytes([167]) * 4096)
            f.truncate(8 * 1024 * 1024)
        env = dict(os.environ, ARIES_DIRTY_LOG='1')
        serial = (out / 'serial.log').open('wb')
        proc = subprocess.Popen([str(binary), '--api-sock', str(sock), '--id', 'aries-dirty-fixture'], stdin=subprocess.PIPE, stdout=serial, stderr=subprocess.STDOUT, env=env)
        for _ in range(100):
            if sock.exists():
                break
            time.sleep(0.05)
        (out / 'metrics.log').touch()
        call('PUT', '/metrics', {'metrics_path': str(out / 'metrics.log')})
        call('PUT', '/machine-config', {'vcpu_count': 1, 'mem_size_mib': mem_mib})
        config = call('GET', '/machine-config')
        assert config['track_dirty_pages'] is True
        call('PUT', '/boot-source', {'kernel_image_path': str(kernel), 'initrd_path': str(initrd), 'boot_args': 'console=ttyS0 reboot=k panic=1 pci=off rdinit=/init'})
        call('PUT', '/drives/blk', {'drive_id': 'blk', 'path_on_host': str(disk), 'is_root_device': False, 'is_read_only': True, 'io_engine': 'Async'})
        call('PUT', '/network-interfaces/net', {'iface_id': 'net', 'host_dev_name': tap, 'guest_mac': '06:00:ac:10:00:02'})
        call('PUT', '/vsock', {'guest_cid': 218, 'uds_path': str(vs)})
        call('PUT', '/actions', {'action_type': 'InstanceStart'})
        m = wait_text('PFNS (\\d+) (\\d+) (\\d+) (\\d+)')
        pfns = list(map(int, m.groups()))
        results['pfns'] = pfns
        wait_text('VSLISTEN')
        conn = socket.socket(socket.AF_UNIX)
        conn.settimeout(10)
        conn.connect(str(vs))
        conn.sendall(b'CONNECT 5000\n')
        results['vsock_connect'] = conn.recv(128).decode()
        wait_text('VSREADY')
        call('PATCH', '/vm', {'state': 'Paused'})
        def export(path, record, kind):
            rows=[]; offset=0
            while True:
                part=call('PUT',path,{'operation':'export','record_id':record,'kind':kind,'offset':offset,'limit':256 if kind=='pages' else 4096})
                rows.extend(part['rows'])
                if part['complete']: return rows
                offset=part['next_offset']
        def ack(record):
            call('PUT','/trace-epochs',{'operation':'ack','record_id':record,'vm_generation':generation})
        arm=call('PUT','/trace-epochs',{'operation':'arm','epoch':1,'phase':'bootstrap','metrics':['dirty','idle','census','metadata']})
        generation=arm['vm_generation']
        results['generation']=generation
        regions=arm['regions']
        ack(arm['epoch_record_id'])
        begin=call('PUT','/metadata-capture',{'operation':'begin','capture_id':'initial','vm_generation':generation,'epoch':1,'mode':'attribution','consistency':'dirty_validated','max_bytes':16384,'max_scan_bytes':0})
        query={'operation':'read_pages','capture_id':'initial','vm_generation':generation,'gpas':[p*4096 for p in pfns]}
        copied=call('PUT','/metadata-capture',query)
        assert len(copied['pages'])==4
        assert all(hashlib.sha256(base64.b64decode(p['data_b64'])).hexdigest()==p['sha256'] for p in copied['pages'])
        denied=call('PATCH','/vm',{'state':'Resumed'},expected=400)
        call('PUT','/memory-census',{'operation':'capture'},expected=400)
        netmarker=b'ARIES_REJECTED_RESUME_NET_286a_unique_payload'
        udp=socket.socket(socket.AF_PACKET,socket.SOCK_RAW)
        udp.bind((tap,0))
        udp.send(bytes.fromhex('0600ac10000202000000000188b5')+netmarker)
        time.sleep(0.2)
        seal=call('PUT','/metadata-capture',{'operation':'seal','capture_id':'initial','vm_generation':generation})
        assert seal['consistency']=='pending'
        call('PATCH','/vm',{'state':'Resumed'},expected=400)
        rotated=call('PUT','/trace-epochs',{'operation':'rotate','epoch':1,'next_epoch':2,'phase':'tool','vm_generation':generation})
        validation=rotated['capture_validation'][0]
        assert validation['consistency']=='valid', validation
        raw=export('/trace-epochs',rotated['epoch_record_id'],'dirty')
        assert not any(p['kvm_dirty'] for p in raw)
        def as_dirty_reply(rows):
            out={'regions':[]}
            for r in regions:
                rr=dict(r)
                for key,field in [('vmm_dirty_b64','vmm_dirty'),('kvm_dirty_b64','kvm_dirty')]:
                    bits=bytearray(((r['page_count']+63)//64)*8)
                    for row in rows:
                        idx=(row['gpa']-r['guest_base'])//4096
                        if row[field] and 0<=idx<r['page_count']: bits[idx//8]|=1<<(idx%8)
                    rr[key]=base64.b64encode(bits).decode()
                out['regions'].append(rr)
            return out
        assert not marker_gpas(as_dirty_reply(raw),netmarker)
        results['rejected_resume_kept_event_loop_held']=True
        ack(rotated['epoch_record_id'])
        call('PATCH','/vm',{'state':'Resumed'})
        frozen=export('/metadata-capture',seal['capture_record_id'],'pages')
        assert [p['data_b64'] for p in frozen]==[p['data_b64'] for p in copied['pages']]
        proc.stdin.write(b'markers\n'); proc.stdin.flush()
        wait_text('MARKERS_DONE read=0 restore=0 block=4096 first=167')
        time.sleep(0.3)
        call('PATCH','/vm',{'state':'Paused'})
        step=call('PUT','/trace-epochs',{'operation':'rotate','epoch':2,'next_epoch':3,'phase':'llm_wait','vm_generation':generation})
        dirty=export('/trace-epochs',step['epoch_record_id'],'dirty')
        access=export('/trace-epochs',step['epoch_record_id'],'access')
        cpu={p['gpa']//4096 for p in dirty if p['kvm_dirty']}
        device={p['gpa']//4096 for p in dirty if p['vmm_dirty']}
        results['markers']={'read_not_cpu_dirty':pfns[0] not in cpu,'write_cpu_dirty':pfns[1] in cpu,'restore_cpu_dirty':pfns[2] in cpu,'block_device_dirty':pfns[3] in device}
        assert all(results['markers'].values()),results['markers']
        results['marker_access_rows']=[p for p in access if p['gpa']//4096 in pfns]
        assert len(results['marker_access_rows'])==4
        results['resumed_net_marker_gpas']=marker_gpas(as_dirty_reply(dirty),netmarker)
        assert results['resumed_net_marker_gpas']
        results['capture_validated']=validation
        results['frozen_export_after_resume']=True
        old=call('PUT','/metadata-capture',{'operation':'info','capture_id':'initial','vm_generation':generation})
        assert old['consistency']=='valid'
        ack(seal['capture_record_id']); ack(old['capture_record_id']); ack(step['epoch_record_id'])
        final=call('PUT','/trace-epochs',{'operation':'rotate','epoch':3,'vm_generation':generation})
        ack(final['epoch_record_id'])
        call('PATCH','/vm',{'state':'Resumed'})
        call('PUT', '/actions', {'action_type': 'FlushMetrics'})
        results['passed'] = True
    except Exception as e:
        results['passed'] = False
        results['error'] = repr(e)
        raise
    finally:
        if conn:
            conn.close()
        if udp:
            udp.close()
        if proc and proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=20)
        if tap_created:
            subprocess.run(['ip', 'link', 'delete', tap], check=True)
        results['cleanup'] = {'process_exited': proc is None or proc.poll() is not None, 'tap_removed': tap_created}
        (out / 'api-calls.json').write_text(json.dumps(calls, indent=2) + '\n')
        (out / 'result.json').write_text(json.dumps(results, indent=2) + '\n')
if __name__ == '__main__':
    p = argparse.ArgumentParser(description='Disposable integrated metric/capture fixture; requires root, KVM, iproute2 and an externally supplied Linux kernel.')
    p.add_argument('--out', type=pathlib.Path, required=True)
    p.add_argument('--binary', type=pathlib.Path, required=True)
    p.add_argument('--kernel', type=pathlib.Path, required=True)
    p.add_argument('--kernel-source', required=True, help='Kernel source/build provenance URL or immutable reference, retained with image SHA256')
    p.add_argument('--initrd', type=pathlib.Path, required=True)
    p.add_argument('--tap', default='ariesdirty')
    p.add_argument('--mem-mib', type=int, choices=[256, 8192], default=256)
    a = p.parse_args()
    run(a.out.resolve(), a.mem_mib, a.binary.resolve(), a.kernel.resolve(), a.initrd.resolve(), a.kernel_source, a.tap)
