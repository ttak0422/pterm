#!/usr/bin/env python3
"""Measure synthetic TUI updates over real pterm PTY/daemon protocol, not a GUI.

Usage: python3 tests/measure_managed_views.py /tmp/pterm-before target/release/pterm
Reports measured wire traffic and input-to-first-VIEW latency. The fixed receive
window is measurement pacing, not application latency. Requires local Unix sockets.
"""
import json
import os
from pathlib import Path
import select
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import tty


def fixture(ready):
    tty.setraw(0)
    Path(ready).touch()
    tick = 0
    while command := os.read(0, 1):
        if command == b'R':
            tick = 0
            body = '\x1b[?1049h\x1b[2J' + ''.join(
                f'\x1b[{row};1H' + ('HEADER' if row == 1 else 'FOOTER' if row == 24 else f'row {row:02} item {row:04}')
                for row in range(1, 25)) + '\x1b[24;1H'
        elif command == b'N':
            body = '\x1b[24;1H'  # Parser output, no visual or cursor change.
        elif command == b'C':
            tick += 1
            body = f'\x1b[24;{1 + tick % 60}H'
        elif command == b'S':
            tick += 1
            body = ''.join(f'\x1b[{row};1H\x1b[2Krow {row:02} item {row + tick:04}'
                           for row in range(2, 24)) + '\x1b[24;1H'
        elif command == b'L':
            tick += 1
            body = f'\x1b[12;1H\x1b[2Kchanged item {tick:04}\x1b[24;1H'
        else:
            continue
        os.write(1, ('\x1b[?2026h' + body + '\x1b[?2026l').encode())


def frame(kind, payload=b''):
    return bytes([kind]) + struct.pack('<I', len(payload)) + payload


def measure(binary):
    binary = str(Path(binary).resolve())
    with tempfile.TemporaryDirectory(prefix='pterm-measure-') as root:
        env = dict(os.environ, PTERM_SOCKET_DIR=root)
        ready = Path(root) / 'ready'
        subprocess.run([binary, 'new', 'measure', '--', sys.executable,
                        str(Path(__file__).resolve()), '--fixture', str(ready)],
                       env=env, check=True, capture_output=True)
        try:
            path = Path(root) / 'measure/socket'
            deadline = time.monotonic() + 5
            while not (ready.exists() and path.exists()):
                if time.monotonic() >= deadline:
                    raise RuntimeError('fixture startup timed out')
                time.sleep(.01)
            with socket.socket(socket.AF_UNIX) as sock:
                sock.connect(str(path))
                buffered = bytearray()

                def receive(duration=.05):
                    frames = []
                    deadline = time.monotonic() + duration
                    while (remaining := deadline - time.monotonic()) > 0:
                        if not select.select([sock], [], [], remaining)[0]:
                            break
                        data = sock.recv(1024 * 1024)
                        if not data:
                            raise RuntimeError('daemon closed unexpectedly')
                        buffered.extend(data)
                        while len(buffered) >= 5:
                            size = struct.unpack_from('<I', buffered, 1)[0]
                            if len(buffered) < size + 5:
                                break
                            frames.append((buffered[0], bytes(buffered[5:5 + size]), time.monotonic()))
                            del buffered[:5 + size]
                    if buffered:
                        raise RuntimeError('receive window ended with an incomplete frame')
                    return frames

                sock.sendall(frame(7, struct.pack('<II', 2, 2)) + frame(10, struct.pack('<HH', 80, 24)))
                receive(.1)
                result = {'binary': binary, 'iterations': 40, 'receive_window_ms': 50, 'scenarios': {}}
                for name, command in [('unchanged', b'N'), ('cursor', b'C'), ('single_row', b'L'), ('scroll_body', b'S')]:
                    sock.sendall(frame(1, b'R'))
                    receive(.1)
                    samples, latency = [], []
                    for _ in range(40):
                        start = time.monotonic()
                        sock.sendall(frame(1, command))
                        received = receive()
                        samples.extend(received)
                        views = [item for item in received if item[0] == 0x87]
                        if views:
                            latency.append((views[0][2] - start) * 1000)
                    views = [payload[1:] for kind, payload, _ in samples if kind == 0x87]
                    result['scenarios'][name] = {
                        'wire_bytes': sum(len(payload) + 5 for _, payload, _ in samples),
                        'view_frames': len(views),
                        'erase_display': sum(v.count(b'\x1b[2J') for v in views),
                        'erase_line': sum(v.count(b'\x1b[2K') for v in views),
                        'first_view_latency_ms_median': round(statistics.median(latency), 3) if latency else None,
                        'first_view_latency_ms_max': round(max(latency), 3) if latency else None,
                    }
                return result
        finally:
            subprocess.run([binary, 'kill', 'measure'], env=env, capture_output=True)


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--fixture':
        fixture(sys.argv[2])
    elif len(sys.argv) >= 2:
        print(json.dumps([measure(binary) for binary in sys.argv[1:]], indent=2))
    else:
        raise SystemExit(__doc__)
