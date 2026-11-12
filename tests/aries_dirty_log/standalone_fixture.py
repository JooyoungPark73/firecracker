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

def run(out, mem_mib, binary, kernel, initrd, kernel_source, tap, flush_marker_tlb=False, capture_census=True):
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
    results['fixture_marker_tlb_invalidation'] = flush_marker_tlb
    results['metadata_capture_census'] = capture_census
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
        env = dict(os.environ)
        env.pop('ARIES_DIRTY_LOG',None)
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
        assert config['track_dirty_pages'] is False
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
        census=call('PUT','/memory-census',{'operation':'capture','request_id':'first'})
        generation=census['vm_generation']
        call('PUT','/dirty-log',{'operation':'arm','epoch':1},expected=400)
        control_pfn=int(wait_text('CONTROL_PFN (\\d+)').group(1))
        arm=call('PUT','/page-idle',{'operation':'arm','epoch':1,'phase':'tool'})
        results['idle_rotations']=[]
        for epoch in range(1,4):
            before=(out/'serial.log').read_text(errors='replace').count('MARKERS_DONE')
            call('PATCH','/vm',{'state':'Resumed'})
            # Optional deterministic positive control. Plain accesses can stay
            # hidden in secondary TLBs; retain that unmodified default oracle.
            proc.stdin.write(b'markers-flush\n' if flush_marker_tlb else b'markers\n'); proc.stdin.flush()
            deadline=time.monotonic()+20
            while (out/'serial.log').read_text(errors='replace').count('MARKERS_DONE')<=before:
                if time.monotonic()>deadline: raise RuntimeError('marker command timeout')
                time.sleep(.05)
            call('PATCH','/vm',{'state':'Paused'})
            request={'operation':'collect','epoch':epoch,'vm_generation':generation}
            if epoch<3: request.update(next_epoch=epoch+1,phase='tool')
            access=call('PUT','/page-idle',request)
            rows=[]; offset=0
            while True:
                chunk=call('PUT','/page-idle',{'operation':'export','record_id':access['access_record_id'],'kind':'access','offset':offset,'limit':4096})
                rows.extend(chunk['rows'])
                if chunk['complete']: break
                offset=chunk['next_offset']
            known=[r for r in rows if r['gpa']//4096 in pfns]
            control=[r for r in rows if r['gpa']//4096==control_pfn]
            results['idle_rotations'].append({'epoch':epoch,'marker_access_rows':known,'untouched_control':control})
            assert len(known)==4 and all(r['state']=='observed' for r in known)
            assert len(control)==1 and control[0]['state']=='idle'
            call('PUT','/page-idle',{'operation':'ack','record_id':access['access_record_id'],'vm_generation':generation})
        begin=call('PUT','/metadata-capture',{'operation':'begin','capture_id':'standalone','mode':'attribution','consistency':'quiescence_only','max_bytes':4096,'max_scan_bytes':4096,'capture_census':capture_census})
        assert bool(begin['census_record_id']) == capture_census
        page=call('PUT','/metadata-capture',{'operation':'read_pages','capture_id':'standalone','vm_generation':generation,'gpas':[pfns[0]*4096]})['pages'][0]
        scan=call('PUT','/metadata-capture',{'operation':'scan','capture_id':'standalone','vm_generation':generation,'needle_b64':base64.b64encode(b'\0'*16).decode(),'start_gpa':pfns[0]*4096,'end_gpa':(pfns[0]+1)*4096,'cursor':pfns[0]*4096,'max_scan_bytes':4096,'resident_only':True})
        assert scan['matches'] and scan['scanned_bytes']==4096
        assert scan['scan_residency_source'] == ('begin_census' if capture_census else 'per_request_pagemap')
        results['scan_residency_source'] = scan['scan_residency_source']
        seal=call('PUT','/metadata-capture',{'operation':'seal','capture_id':'standalone','vm_generation':generation})
        assert seal['consistency']=='quiescence_only'
        call('PATCH','/vm',{'state':'Resumed'})
        frozen=call('PUT','/metadata-capture',{'operation':'export','record_id':seal['capture_record_id'],'kind':'pages','offset':0,'limit':256})
        assert frozen['rows'][0]['sha256']==page['sha256']
        mask=call('PUT','/metadata-capture',{'operation':'export','record_id':seal['capture_record_id'],'kind':'read_mask','offset':0,'limit':256})
        assert len(mask['rows'])==1 and mask['rows'][0]['metadata_read'] and mask['rows'][0]['scan_read']
        results['dirty_disabled_independent_metrics']=True
        results['scan_and_payload_read_mask']=mask['rows']
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
    p.add_argument('--flush-marker-tlb', action='store_true', help='fixture-only mprotect positive control; does not prove arbitrary read coverage')
    p.add_argument('--no-capture-census', action='store_true', help='scan and read metadata with per-request presence probes, without a capture census')
    a = p.parse_args()
    run(a.out.resolve(), a.mem_mib, a.binary.resolve(), a.kernel.resolve(), a.initrd.resolve(), a.kernel_source, a.tap, a.flush_marker_tlb, not a.no_capture_census)
