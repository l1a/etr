#!/usr/bin/env python3
"""TCP bidirectional pump for stress-local. Usage: tcp_pump.py PORT"""
import os
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


def drain():
    while True:
        try:
            sock.recv(65536)
        except Exception:
            break


threading.Thread(target=drain, daemon=True).start()
while True:
    try:
        sock.sendall(chunk)
    except Exception:
        break
