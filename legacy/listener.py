#!/usr/bin/env python3
"""
Packet-size probe — the original prototype, kept as a diagnostic.

Superseded by the Rust ingest layer, but this remains the fastest way to answer
"is anything reaching this machine at all, and what is it?" before debugging
anything more complicated.

The packet LENGTH alone identifies the simulator and, for Forza, the exact
title — which is the first thing you want to know when a decoder produces
nonsense.

    python3 legacy/listener.py [--port 5000]
"""

import argparse
import collections
import socket
import time

KNOWN = {
    232:  "Forza Motorsport 7 (Sled)",
    311:  "Forza Motorsport 7 (Car Dash)",
    323:  "Forza Horizon 4/5",
    324:  "Forza Horizon 4/5",
    331:  "Forza Motorsport 2023",
    328:  "Assetto Corsa RTCarInfo",
    408:  "Assetto Corsa handshake response",
    1349: "F1 25 Motion",
    1352: "F1 25 CarTelemetry",
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=5000)
    args = ap.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", args.port))
    print(f"Listening on UDP {args.port}. Start driving; ctrl-c to stop.\n")

    seen = collections.Counter()
    start = time.time()
    last = start
    try:
        while True:
            data, addr = sock.recvfrom(4096)
            seen[len(data)] += 1
            now = time.time()
            if now - last >= 1.0:
                elapsed = now - start
                print(f"[{elapsed:5.1f}s] from {addr[0]}")
                for size, count in seen.most_common():
                    name = KNOWN.get(size, "unrecognised")
                    print(f"    {size:>5} bytes  x{count:<6}  {name}  "
                          f"({count / elapsed:.0f} Hz)")
                last = now
    except KeyboardInterrupt:
        print("\nstopped.")


if __name__ == "__main__":
    main()
