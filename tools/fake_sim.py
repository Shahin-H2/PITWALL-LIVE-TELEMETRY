#!/usr/bin/env python3
"""
Synthetic telemetry source — emits real wire-format packets for Forza, F1 25,
and Assetto Corsa so the app can be developed and demoed without owning, or
running, any of the three simulators.

This is a test fixture that speaks the actual protocols, byte for byte. If a
packet from here decodes correctly in pitwall, the decoder handles the real
thing too — which is the point: it is a protocol conformance harness, not a
mock.

The driving model is a simple but not trivial lap: two straights and four
corners, with braking zones, trail braking, gear changes tied to engine speed,
and a rear-biased slip balance so the balance analysis has something to find.

    python3 tools/fake_sim.py --sim forza
    python3 tools/fake_sim.py --sim f1 --port 20777
    python3 tools/fake_sim.py --sim forza --laps 3 --hz 60
"""

import argparse
import math
import socket
import struct
import sys
import time

# ---------------------------------------------------------------- lap model

# (name, seconds, entry_speed, apex_speed, corner_radius_sign)
# A sign of 0 marks a straight.
SEGMENTS = [
    ("start straight", 6.0, 0),
    ("turn 1 right",   3.2, +1),
    ("short chute",    2.5, 0),
    ("turn 2 left",    4.0, -1),
    ("back straight",  7.5, 0),
    ("turn 3 left",    2.8, -1),
    ("turn 4 right",   3.6, +1),
]
LAP_TIME = sum(s[1] for s in SEGMENTS)

REDLINE = 7800.0
IDLE = 900.0
GEAR_RATIOS = [3.20, 2.10, 1.55, 1.20, 0.98, 0.82]   # 6 speeds
FINAL_DRIVE = 3.7
WHEEL_CIRC_M = 2.0


def lap_state(t):
    """Return the car's state at time `t` seconds into the lap."""
    phase = t % LAP_TIME
    acc = 0.0
    seg = SEGMENTS[-1]
    seg_t = 0.0
    for s in SEGMENTS:
        if phase < acc + s[1]:
            seg, seg_t = s, phase - acc
            break
        acc += s[1]

    name, dur, turn = seg
    u = seg_t / dur                      # 0..1 through the segment

    if turn == 0:
        # Straight: accelerate toward a high top speed, brake at the very end.
        braking = u > 0.80
        speed = 42.0 + 38.0 * min(u / 0.80, 1.0)
        if braking:
            b = (u - 0.80) / 0.20
            speed = 80.0 - 38.0 * b
        throttle = 0.0 if braking else 1.0
        brake = 0.95 * ((u - 0.80) / 0.20) if braking else 0.0
        steer = 0.0
        lat_g = 0.0
    else:
        # Corner: decelerate to the apex, then power out.
        if u < 0.35:                      # entry, trailing brake
            k = u / 0.35
            speed = 62.0 - 24.0 * k
            throttle = 0.0
            brake = 0.55 * (1.0 - k)      # trail braking
        elif u < 0.50:                    # apex, brief coast
            speed = 38.0
            throttle = 0.0
            brake = 0.0
        else:                             # exit
            k = (u - 0.50) / 0.50
            speed = 38.0 + 26.0 * k
            throttle = min(1.0, k * 1.6)
            brake = 0.0
        # Lateral load peaks near the apex.
        lat_g = turn * 1.9 * math.sin(math.pi * min(u * 1.15, 1.0))
        steer = turn * 0.42 * math.sin(math.pi * min(u * 1.15, 1.0))

    # Longitudinal g from the pedal state.
    long_g = throttle * 0.9 - brake * 1.7

    # Gear from speed, choosing the ratio that lands nearest 75% of redline.
    gear = 1
    for i, ratio in enumerate(GEAR_RATIOS, start=1):
        rpm = (speed / WHEEL_CIRC_M) * ratio * FINAL_DRIVE * 60.0
        if rpm < REDLINE * 0.95:
            gear = i
            break
        gear = i
    rpm = (speed / WHEEL_CIRC_M) * GEAR_RATIOS[gear - 1] * FINAL_DRIVE * 60.0
    rpm = max(IDLE, min(REDLINE, rpm))

    # Slip angles, radians. Deliberately rear-biased under power and
    # front-biased on entry, so the balance analysis has a real signature to
    # detect rather than symmetric noise.
    base = abs(lat_g) * 0.02
    front = base * (1.25 if brake > 0.1 else 0.85)
    rear = base * (1.35 if throttle > 0.5 else 0.80)
    sign = 1.0 if turn >= 0 else -1.0
    slip_angle = [sign * front, sign * front, sign * rear, sign * rear]
    slip_ratio = [0.0, 0.0, throttle * 0.06, throttle * 0.06]

    return dict(
        speed=speed, rpm=rpm, gear=gear, throttle=throttle, brake=brake,
        steer=steer, lat_g=lat_g, long_g=long_g,
        slip_angle=slip_angle, slip_ratio=slip_ratio,
        lap_distance=phase / LAP_TIME, name=name,
    )


# ------------------------------------------------------------ wire formats

def forza_packet(s, t_ms):
    """Forza Horizon 'Dash' packet: 324 bytes, Sled + 12 unknown + Dash."""
    b = bytearray(324)
    struct.pack_into("<i", b, 0, 1)                    # isRaceOn
    struct.pack_into("<I", b, 4, t_ms)
    struct.pack_into("<f", b, 8, REDLINE)
    struct.pack_into("<f", b, 12, IDLE)
    struct.pack_into("<f", b, 16, s["rpm"])
    # accel x,y,z (m/s^2)
    struct.pack_into("<fff", b, 20, s["lat_g"] * 9.80665, 0.0, s["long_g"] * 9.80665)
    struct.pack_into("<fff", b, 32, 0.0, 0.0, s["speed"])   # velocity
    # slip ratio / angle, [FL, FR, RL, RR]
    struct.pack_into("<ffff", b, 84, *s["slip_ratio"])
    struct.pack_into("<ffff", b, 164, *s["slip_angle"])

    db = 232 + 12                                      # Horizon dash base
    struct.pack_into("<fff", b, db + 0, 0.0, 0.0, 0.0)  # position
    struct.pack_into("<f", b, db + 12, s["speed"])
    struct.pack_into("<ffff", b, db + 24, 190.0, 190.0, 195.0, 195.0)  # tyre °F
    struct.pack_into("<f", b, db + 48, s["lap_distance"] * 4000.0)
    b[db + 71] = int(s["throttle"] * 255)
    b[db + 72] = int(s["brake"] * 255)
    b[db + 75] = s["gear"]                             # absolute 319
    struct.pack_into("<b", b, db + 76, int(s["steer"] * 127))
    return bytes(b)


def f1_header(packet_id, session_time):
    h = bytearray(29)
    struct.pack_into("<H", h, 0, 2025)                 # m_packetFormat
    h[2] = 25                                          # m_gameYear
    h[6] = packet_id
    struct.pack_into("<f", h, 15, session_time)
    h[27] = 0                                          # m_playerCarIndex
    return h


def f1_telemetry_packet(s, session_time):
    """F1 25 CarTelemetry (id 6): 29 header + 22 x 60 + 3 trailer."""
    stride, cars = 60, 22
    b = bytearray(29 + cars * stride + 3)
    b[:29] = f1_header(6, session_time)
    o = 29                                             # player is car 0
    struct.pack_into("<H", b, o + 0, int(s["speed"] * 3.6))
    struct.pack_into("<f", b, o + 2, s["throttle"])
    struct.pack_into("<f", b, o + 6, s["steer"])
    struct.pack_into("<f", b, o + 10, s["brake"])
    b[o + 14] = 0
    struct.pack_into("<b", b, o + 15, s["gear"])
    struct.pack_into("<H", b, o + 16, int(s["rpm"]))
    # F1 wheel arrays are [RL, RR, FL, FR]
    for i, v in enumerate([90, 90, 95, 95]):
        b[o + 29 + i] = v
    return bytes(b)


def f1_motion_ex_packet(s, session_time):
    """F1 25 MotionEx (id 13): player car only, fixed offsets after header."""
    b = bytearray(29 + 240)
    b[:29] = f1_header(13, session_time)
    o = 29
    fl, fr, rl, rr = s["slip_angle"]
    struct.pack_into("<ffff", b, o + 48, rl, rr, fl, fr)   # slip ratio slot
    struct.pack_into("<ffff", b, o + 64, rl, rr, fl, fr)   # slip angle
    return bytes(b)


def f1_motion_packet(s, session_time):
    stride, cars = 60, 22
    b = bytearray(29 + cars * stride)
    b[:29] = f1_header(0, session_time)
    o = 29
    struct.pack_into("<f", b, o + 36, s["lat_g"])
    struct.pack_into("<f", b, o + 40, s["long_g"])
    return bytes(b)


def ac_packet(s, lap_ms):
    """Assetto Corsa RTCarInfo: 328 bytes."""
    b = bytearray(328)
    b[0] = ord("a")
    struct.pack_into("<i", b, 4, 328)
    struct.pack_into("<f", b, 8, s["speed"] * 3.6)
    struct.pack_into("<f", b, 12, s["speed"] * 2.23694)
    struct.pack_into("<f", b, 16, s["speed"])
    struct.pack_into("<f", b, 28, 0.0)                  # accG vertical
    struct.pack_into("<f", b, 32, s["lat_g"])
    struct.pack_into("<f", b, 36, s["long_g"])
    struct.pack_into("<i", b, 40, lap_ms)
    struct.pack_into("<f", b, 56, s["throttle"])
    struct.pack_into("<f", b, 60, s["brake"])
    struct.pack_into("<f", b, 68, s["rpm"])
    struct.pack_into("<f", b, 72, s["steer"])
    struct.pack_into("<i", b, 76, s["gear"] + 1)        # AC: 0=R, 1=N, 2=1st
    struct.pack_into("<ffff", b, 100, *s["slip_angle"])
    struct.pack_into("<ffff", b, 132, *s["slip_ratio"])
    struct.pack_into("<f", b, 308, s["lap_distance"])
    return bytes(b)


# ------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--sim", choices=["forza", "f1", "ac"], default="forza")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5000)
    ap.add_argument("--hz", type=float, default=60.0)
    ap.add_argument("--laps", type=float, default=0.0,
                    help="stop after N laps (0 = run forever)")
    args = ap.parse_args()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    dest = (args.host, args.port)
    period = 1.0 / args.hz

    print(f"transmitting {args.sim} packets to {args.host}:{args.port} at {args.hz:g} Hz")
    print(f"lap time {LAP_TIME:.1f}s over {len(SEGMENTS)} segments; ctrl-c to stop")

    t0 = time.perf_counter()
    n = 0
    last_seg = None
    try:
        while True:
            now = time.perf_counter() - t0
            if args.laps and now > args.laps * LAP_TIME:
                break

            s = lap_state(now)
            if s["name"] != last_seg:
                print(f"  [{now:6.1f}s] {s['name']}")
                last_seg = s["name"]

            if args.sim == "forza":
                sock.sendto(forza_packet(s, int(now * 1000)), dest)
            elif args.sim == "f1":
                # Mirror the real title: several packet types interleaved,
                # with CarTelemetry as the heartbeat.
                sock.sendto(f1_motion_packet(s, now), dest)
                sock.sendto(f1_motion_ex_packet(s, now), dest)
                sock.sendto(f1_telemetry_packet(s, now), dest)
            else:
                sock.sendto(ac_packet(s, int((now % LAP_TIME) * 1000)), dest)

            n += 1
            target = t0 + n * period
            sleep = target - time.perf_counter()
            if sleep > 0:
                time.sleep(sleep)
    except KeyboardInterrupt:
        pass
    print(f"\nsent {n} ticks over {time.perf_counter() - t0:.1f}s")


if __name__ == "__main__":
    sys.exit(main())
