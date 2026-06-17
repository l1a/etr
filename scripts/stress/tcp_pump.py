#!/usr/bin/env python3
"""TCP bidirectional pump for stress-local. Usage: tcp_pump.py PORT"""
import os
import socket
import sys
import threading

port = int(sys.argv[1])
sock = socket.socket()
sock.connect(("127.0.0.1", port))
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
