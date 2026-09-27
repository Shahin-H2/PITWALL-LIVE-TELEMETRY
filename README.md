# Pitwall

Live racing telemetry and setup analysis for **Forza**, **F1 25**, and
**Assetto Corsa**. Rust, GPU-rendered, no garbage collector anywhere in the
frame path.

```
crates/telemetry-core       UDP ingest, protocol decoders, lock-free transport, capture
crates/telemetry-analysis   corner segmentation, driver fingerprint, gearing, setup advice
crates/pitwall              the application: wgpu renderer, audio, CLI
ai/tune_engineer.py         Claude layer — prioritises and explains the physics layer
tools/fake_sim.py           synthetic packet source, all three protocols
legacy/                     the original C and Python prototypes (engine.c bug fixed)
```

## Quick start

You don't need a simulator to see it run.

```sh
# terminal 1 — synthetic telemetry
python3 tools/fake_sim.py --sim forza

# terminal 2 — the cluster
cargo run --release -p pitwall
```

`--sim f1` and `--sim ac` speak the other two protocols. The app auto-detects
which; you never tell it.

## Against a real sim

| Sim | Configure it to send to | Notes |
|---|---|---|
| Forza Horizon 4/5, Motorsport | this machine's IP, port 5000, "Data Out" = ON, format **Dash** | |
| F1 25 | Settings → Telemetry, UDP on, port 5000, rate **60 Hz** | |
| Assetto Corsa | — | Pass `--ac-server <sim-ip>:9996`; AC is request/response and must be asked |

```sh
cargo run --release -p pitwall -- --port 5000
cargo run --release -p pitwall -- --ac-server 192.168.1.20:9996
```

**ACC is different.** Its UDP feed carries session and timing data, not
physics. The real ACC channel is shared memory, readable only on the machine
running the sim, so a remote rig needs a small forwarder. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Record, replay, analyse

Recording is not a nice-to-have. Without it, every UI tweak and every threshold
change costs a real lap in a real sim.

```sh
# capture 90s headless (no window, no GPU) on the sim machine
pitwall --capture 90 --record captures/quali.pwtl

# drive the UI from that capture instead of the network
pitwall --replay captures/quali.pwtl

# engineering report, plus JSON for the AI layer
pitwall --analyse captures/quali.pwtl --json captures/quali.json
```

Captures store **raw wire bytes**, not decoded samples, so a capture taken
today can be re-decoded after a protocol fix — and doubles as a regression
corpus for the decoders.

## The AI race engineer

```sh
pip install anthropic
export ANTHROPIC_API_KEY=...          # or: ant auth login

python3 ai/tune_engineer.py captures/quali.json --track "Spa" --car "GT3"
python3 ai/tune_engineer.py captures/quali.json --offline   # no model, still complete
```

The split matters:

- **`telemetry-analysis` (Rust) decides what changes and in which direction.**
  Deterministic, offline, instant, and every recommendation carries the
  measurement that produced it.
- **Claude prioritises and explains**, given those conclusions as fact. It is
  instructed not to invent parameters, and `validate()` drops any that appear
  anyway.

Claude never produces a number that goes into the car. If the API is
unreachable, `--offline` output is a complete engineering report — the model
makes it better, it is not load-bearing.

## Keys

The app opens on a standby screen. **Any key** (or a click) starts the ignition
sequence into the dashboard.

| Key | |
|---|---|
| any | initialize — runs the boot sequence |
| `Tab` | cycle the pinned simulator |
| `P` | toggle pin / auto-detect |
| `Esc` | quit |

## Screenshots

Frames can be rendered headlessly, with no window and no display — for review,
for docs, and for visual regression in CI.

```sh
pitwall --screenshot shot.png --size 3840x2160
pitwall --screenshot standby.png --ignition-at 1.0     # the standby screen
pitwall --screenshot mid.png     --ignition-at 0.35    # mid-transition
pitwall --screenshot hover.png   --hover               # a panel in hover state
pitwall --screenshot lap.png     --replay captures/example.pwtl
```

`--ignition-at` scrubs the boot sequence, so any instant of the transition can
be rendered and diffed.

Samples live in [docs/shots/](docs/shots/).

## Build and test

```sh
cargo build --release
cargo test              # 78 tests
```

The frame-time histogram is on screen permanently, not behind a debug flag.
The whole architecture is a bet on frame consistency; if that bet is ever lost
it should be visible immediately, not discovered in a bug report.
