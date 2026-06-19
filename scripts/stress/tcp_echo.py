#!/usr/bin/env python3
"""TCP echo server for stress-local. Usage: tcp_echo.py PORT"""
import socket
import sys
import threading


def echo(conn):
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            conn.sendall(data)
    except Exception:
        pass
    finally:
        conn.close()


port = int(sys.argv[1])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(20)
while True:
    conn, _ = srv.accept()
    threading.Thread(target=echo, args=(conn,), daemon=True).start()
