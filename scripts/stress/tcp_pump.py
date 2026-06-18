#!/usr/bin/env python3
"""TCP bidirectional pump for stress-local. Usage: tcp_pump.py PORT

On SIGTERM (or natural exit) prints one line to stdout:
  TCP sent=<bytes> recv=<bytes> elapsed=<seconds>
which stress-local reads to compute Mb/s.
"""
import os
import signal
import socket
import sys
import threading
import time

port = int(sys.argv[1])
sock = socket.socket()
# Retry for up to 5 s — the -L listener binds asynchronously after session setup.
for _ in range(50):
    try:
        sock.connect(("127.0.0.1", port))
        break
    except ConnectionRefusedError:
        time.sleep(0.1)
else:
    sys.exit(f"tcp_pump: could not connect to 127.0.0.1:{port} after 5s")
sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
chunk = os.urandom(65536)

bytes_sent = 0
bytes_recv = 0
lock = threading.Lock()
start = time.monotonic()


def drain():
    global bytes_recv
    while True:
        try:
            data = sock.recv(65536)
            if not data:
                break
            with lock:
                bytes_recv += len(data)
        except Exception:
            break


threading.Thread(target=drain, daemon=True).start()


def report(signum=None, frame=None):
    elapsed = max(time.monotonic() - start, 0.001)
    with lock:
        s, r = bytes_sent, bytes_recv
    print(f"TCP sent={s} recv={r} elapsed={elapsed:.3f}", flush=True)
    sys.exit(0)


signal.signal(signal.SIGTERM, report)

while True:
    try:
        sock.sendall(chunk)
        with lock:
            bytes_sent += len(chunk)
    except Exception:
        break

report()
