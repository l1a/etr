#!/usr/bin/env python3
"""UDP echo server for stress-local. Usage: udp_echo.py PORT"""
import socket
import sys

port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
while True:
    data, addr = srv.recvfrom(65535)
    try:
        srv.sendto(data, addr)
    except Exception:
        pass
