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

    def call(method, path, body=None):
        c = Conn(sock)
        c.request(method, path, json.dumps(body) if body is not None else None, {'Content-Type': 'application/json'})
        r = c.getresponse()
        raw = r.read()
        c.close()
        d = json.loads(raw) if raw else None
        calls.append({'method': method, 'path': path, 'body': body, 'status': r.status, 'response': d})
        if r.status >= 400:
            raise RuntimeError((path, r.status, d))
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
        arm = call('PUT', '/dirty-log', {'operation': 'arm', 'epoch': 1})
        results['memory_layout'] = [{k: r[k] for k in ('slot', 'guest_base', 'bytes', 'page_count')} for r in arm['regions']]
        assert sum((r['bytes'] for r in arm['regions'])) == mem_mib * 1024 * 1024
        if mem_mib == 8192:
            assert len(arm['regions']) == 2
        call('PATCH', '/vm', {'state': 'Resumed'})
        assert arm == call('PUT', '/dirty-log', {'operation':'arm','epoch':1})
        results['immutable_retry_after_resume'] = True
        try:
            call('PUT','/dirty-log',{'operation':'collect','epoch':1,'next_epoch':2})
        except RuntimeError as error:
            assert error.args[0][1] == 400
        else:
            raise AssertionError('fresh running dirty collection must fail')
        results['fresh_running_collect_rejected'] = True
        proc.stdin.write(b'markers\n')
        proc.stdin.flush()
        wait_text('MARKERS_DONE read=0 restore=0 block=4096 first=167')
        call('PATCH', '/vm', {'state': 'Paused'})
        # A pause/resume cycle must preserve both CPU and device epoch evidence.
        call('PATCH', '/vm', {'state': 'Resumed'})
        call('PATCH', '/vm', {'state': 'Paused'})
        marks = call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 1, 'next_epoch': 2})
        cpu = dirty_pages(marks, 'kvm_dirty_b64')
        vmm = dirty_pages(marks, 'vmm_dirty_b64')
        results['markers'] = {'read_not_cpu_dirty': pfns[0] not in cpu, 'write_cpu_dirty': pfns[1] in cpu, 'restored_cpu_dirty': pfns[2] in cpu, 'block_dma_vmm_dirty': pfns[3] in vmm}
        assert all(results['markers'].values()), results
        results['pause_resume_preserves_epoch_bits'] = True
        assert marks == call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 1, 'next_epoch': 2})
        clean = call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 2, 'next_epoch': 3})
        assert not dirty_pages(clean, 'kvm_dirty_b64')
        results['paused_reset_cpu_empty'] = True
        if mem_mib == 8192:
            results['scope'] = '8GiB two-slot CPU/block markers and read/reset; net/vsock payload validated by 256MiB fixture'
            results['passed'] = True
            return
        netmarker = b'ARIES_PAUSED_NET_7f864dac_d589_unique_payload'
        udp = socket.socket(socket.AF_PACKET, socket.SOCK_RAW)
        udp.bind((tap, 0))
        udp.send(bytes.fromhex('0600ac10000202000000000188b5') + netmarker)
        time.sleep(0.2)
        net = call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 3, 'next_epoch': 4})
        results['paused_net_marker_gpas'] = marker_gpas(net, netmarker)
        assert not results['paused_net_marker_gpas']
        assert not dirty_pages(net, 'kvm_dirty_b64')
        call('PATCH', '/vm', {'state': 'Resumed'})
        time.sleep(0.05)
        call('PATCH', '/vm', {'state': 'Paused'})
        net = call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 4, 'next_epoch': 5})
        results['resumed_net_marker_gpas'] = marker_gpas(net, netmarker)
        assert results['resumed_net_marker_gpas']
        vsmarker = b'ARIES_PAUSED_VSOCK_b4d17086_1f36_unique_payload'
        conn.sendall(vsmarker)
        time.sleep(0.2)
        vsdirty = call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 5, 'next_epoch': 6})
        results['paused_vsock_marker_gpas'] = marker_gpas(vsdirty, vsmarker)
        assert not results['paused_vsock_marker_gpas']
        assert not dirty_pages(vsdirty, 'kvm_dirty_b64')
        call('PATCH', '/vm', {'state': 'Resumed'})
        time.sleep(0.05)
        call('PATCH', '/vm', {'state': 'Paused'})
        vsdirty = call('PUT', '/dirty-log', {'operation': 'collect', 'epoch': 6, 'next_epoch': 7})
        results['resumed_vsock_marker_gpas'] = marker_gpas(vsdirty, vsmarker)
        assert results['resumed_vsock_marker_gpas']
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
    p = argparse.ArgumentParser(description='Disposable native dirty-epoch fixture; requires root, KVM, iproute2 and an externally supplied Linux kernel.')
    p.add_argument('--out', type=pathlib.Path, required=True)
    p.add_argument('--binary', type=pathlib.Path, required=True)
    p.add_argument('--kernel', type=pathlib.Path, required=True)
    p.add_argument('--kernel-source', required=True, help='Kernel source/build provenance URL or immutable reference, retained with image SHA256')
    p.add_argument('--initrd', type=pathlib.Path, required=True)
    p.add_argument('--tap', default='ariesdirty')
    p.add_argument('--mem-mib', type=int, choices=[256, 8192], default=256)
    a = p.parse_args()
    run(a.out.resolve(), a.mem_mib, a.binary.resolve(), a.kernel.resolve(), a.initrd.resolve(), a.kernel_source, a.tap)
