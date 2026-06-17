#!/usr/bin/env python3
"""UDP bidirectional pump for stress-local. Usage: udp_pump.py PORT"""
import os
import socket
import sys
import threading
import time

port = int(sys.argv[1])
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.settimeout(0.5)
chunk = os.urandom(1400)


def drain():
    while True:
        try:
            sock.recv(65535)
        except socket.timeout:
            pass
        except Exception:
            break


threading.Thread(target=drain, daemon=True).start()
while True:
    try:
        sock.sendto(chunk, ("127.0.0.1", port))
    except Exception:
        break
    time.sleep(0.001)
