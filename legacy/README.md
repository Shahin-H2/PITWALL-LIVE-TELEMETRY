# Legacy prototypes

The two files this project started from. Both are superseded by the Rust
workspace, and both are kept deliberately.

## `engine.c`

The original C listener, **with the gear-offset bug fixed**. It read `gear` at
absolute byte 315, which in the Forza Horizon layout is the throttle byte — so
"gear" swept 0–255 with the right foot instead of stepping with the shifter.
The correct Horizon offset is 319. The file's header comment explains the
12-byte Sled/Dash shift that causes this.

Still useful as a 60-second sanity check: if this prints plausible numbers,
the sim is transmitting and the network path is clear.

```sh
cc -O2 -o output/engine legacy/engine.c && ./output/engine
```

## `listener.py`

The original packet-size probe. Its real value is diagnostic: run it, drive,
and read the reported packet length to identify which sim and which variant is
transmitting.

```sh
python3 legacy/listener.py
```

| Bytes | Sim |
|-------|-----|
| 232   | Forza Motorsport 7, "Sled" mode |
| 311   | Forza Motorsport 7, "Car Dash" mode |
| 324   | Forza Horizon 4 / 5 |
| 331   | Forza Motorsport (2023) |
| 328   | Assetto Corsa `RTCarInfo` |
| 1352  | F1 25 CarTelemetry |
| 1349  | F1 25 Motion |
