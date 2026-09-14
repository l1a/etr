#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 l1a
"""Turn two one-way SINK lines into a comparison you can act on.

Takes the raw-path baseline and the through-etr measurement and reports both plus the ratio.

WHY A BASELINE IS MANDATORY HERE
--------------------------------
A throughput number on its own cannot say whether etr or the network is the limit, and this
project has already drawn a wrong conclusion from one. Reporting the pair, always, is what
makes the number answerable -- if etr is at 95% of the raw path there is nothing to optimise,
and if it is at 30% there is.
"""

from __future__ import annotations

import sys


def parse(line: str) -> tuple[int, float] | None:
    """Pull `recv=<bytes> elapsed=<seconds>` out of a SINK line."""
    if not line or not line.strip().startswith("SINK"):
        return None
    fields = {}
    for token in line.split()[1:]:
        key, _, value = token.partition("=")
        fields[key] = value
    try:
        return int(fields["recv"]), float(fields["elapsed"])
    except (KeyError, ValueError):
        return None


def mbps(recv: int, elapsed: float) -> float:
    return recv * 8 / elapsed / 1e6 if elapsed > 0 else 0.0


def main() -> int:
    base = parse(sys.argv[1] if len(sys.argv) > 1 else "")
    etr = parse(sys.argv[2] if len(sys.argv) > 2 else "")

    print("==> One-way TCP goodput (what the receiver actually got)")
    if base is None:
        print("  raw TCP  : no measurement -- the baseline sink produced no SINK line")
    else:
        print(f"  raw TCP  : {mbps(*base):8.1f} Mb/s   ({base[0] / 1e6:.0f} MB in {base[1]:.1f}s)")
    if etr is None:
        print("  via etr  : no measurement -- the etr sink produced no SINK line")
    else:
        print(f"  via etr  : {mbps(*etr):8.1f} Mb/s   ({etr[0] / 1e6:.0f} MB in {etr[1]:.1f}s)")

    if base is None or etr is None:
        print()
        print("  A missing side makes the other unanswerable -- do not quote one alone.")
        return 1

    b, e = mbps(*base), mbps(*etr)
    if b <= 0:
        print("\n  baseline is zero; nothing to compare against")
        return 1
    pct = e / b * 100
    print(f"\n  etr reaches {pct:.0f}% of the single-stream TCP baseline")

    # Interpretation, because the number alone invites the wrong reading -- in BOTH directions.
    if pct > 110:
        verdict = (
            "etr is FASTER than the baseline. That is not etr beating the network: the\n"
            "  baseline is one untuned TCP stream, and QUIC's loss recovery can genuinely beat\n"
            "  that on a lossy path. Measured between two wired hosts here: 505 vs 274 Mb/s on\n"
            "  a link that nuttcp showed retransmitting. Do not read this as a path capacity."
        )
    elif pct >= 85:
        verdict = "etr is not the bottleneck here -- the path is. Optimising etr will not help."
    elif pct >= 50:
        verdict = "etr costs a real fraction of the path. Worth investigating, not alarming."
    else:
        verdict = (
            "etr is well short of the baseline. Worth investigating -- but first check that the\n"
            "  comparison is fair (same direction, duration, host pair and BINARIES, run\n"
            "  back-to-back). A stale baseline is how a bogus ratio gets quoted."
        )
    print(f"  {verdict}")

    print()
    print("  NOTE: this is ONE-WAY goodput, comparable to `iperf3 -c` or `nuttcp`. The figures")
    print("  from `stress-local` and `stress-udp-rate` are ECHO workloads and are NOT")
    print("  comparable -- they count every byte twice and report offered load, not goodput.")
    print()
    print("  The baseline is ONE untuned TCP stream, which is a floor rather than the path's")
    print("  capacity. For a real capacity figure use `iperf3`/`nuttcp` with parallel streams;")
    print("  this tool's value is that both numbers come from the same code, back to back.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
