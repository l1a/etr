#!/usr/bin/env python3
"""UDP bidirectional pump for stress-local. Usage: udp_pump.py PORT

On SIGTERM (or natural exit) prints one line to stdout:
  UDP sent=<bytes> recv=<bytes> elapsed=<seconds>
which stress-local reads to compute Mb/s.
"""
import os
import signal
import socket
import sys
import threading
import time

port = int(sys.argv[1])
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.settimeout(0.5)
chunk = os.urandom(1400)

bytes_sent = 0
bytes_recv = 0
lock = threading.Lock()
start = time.monotonic()


def drain():
    global bytes_recv
    while True:
        try:
            data = sock.recv(65535)
            with lock:
                bytes_recv += len(data)
        except socket.timeout:
            pass
        except Exception:
            break


threading.Thread(target=drain, daemon=True).start()


def report(signum=None, frame=None):
    elapsed = max(time.monotonic() - start, 0.001)
    with lock:
        s, r = bytes_sent, bytes_recv
    print(f"UDP sent={s} recv={r} elapsed={elapsed:.3f}", flush=True)
    sys.exit(0)


signal.signal(signal.SIGTERM, report)

while True:
    try:
        sock.sendto(chunk, ("127.0.0.1", port))
        with lock:
            bytes_sent += len(chunk)
    except Exception:
        break
    time.sleep(0.001)

report()
