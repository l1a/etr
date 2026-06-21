#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Regression test for v0.4.9 per-sender UDP routing fix.
#
# Sends interleaved datagrams from two independent sockets through the
# forwarded UDP port and asserts each socket receives its own reply.
# Before v0.4.9 the last-sender-wins bug would cause replies to go to
# whichever sender sent most recently, not the socket that sent the datagram.
#
# Usage: udp_concurrent_senders.py <fwd_port>

import socket
import sys
import threading
import time

FWD_PORT = int(sys.argv[1])

results: dict[str, bytes] = {}
errors: list[str] = []


def sender(name: str, payload: bytes, delay: float) -> None:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(5.0)
    try:
        time.sleep(delay)
        s.sendto(payload, ("127.0.0.1", FWD_PORT))
        data, _ = s.recvfrom(1024)
        results[name] = data
    except Exception as e:
        errors.append(f"{name}: {e}")
    finally:
        s.close()


payload_a = b"CONCURRENT_SENDER_A_PAYLOAD"
payload_b = b"CONCURRENT_SENDER_B_PAYLOAD"

t_a = threading.Thread(target=sender, args=("A", payload_a, 0.0))
t_b = threading.Thread(target=sender, args=("B", payload_b, 0.05))
t_a.start()
t_b.start()
t_a.join(timeout=8)
t_b.join(timeout=8)

if errors:
    for e in errors:
        print(f"ERROR: {e}", file=sys.stderr)
    sys.exit(1)

ok_a = results.get("A") == payload_a
ok_b = results.get("B") == payload_b

if ok_a and ok_b:
    print("PASS")
else:
    if not ok_a:
        print(
            f"FAIL_A: got {results.get('A')!r} expected {payload_a!r}", file=sys.stderr
        )
    if not ok_b:
        print(
            f"FAIL_B: got {results.get('B')!r} expected {payload_b!r}", file=sys.stderr
        )
    sys.exit(1)
