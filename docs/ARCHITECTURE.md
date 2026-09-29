# Architecture

## The latency budget, honestly

"Zero latency" is not achievable and, more usefully, is aimed at the wrong
target. Glass-to-glass:

| Stage | Time | Who controls it |
|---|---|---|
| Sim physics tick → UDP send | **8–16 ms** | The sim. Not us. |
| LAN transit | 0.1–0.5 ms | Physics |
| Kernel → userspace (`recvfrom`) | 5–20 µs | Marginal |
| **Decode 324 bytes** | **~0.1 µs** | Us — and already free |
| Animation + draw-list build | 10–40 µs | Us |
| **Present queue + vsync** | **16–50 ms** | **Us — and this is everything** |

The decoder is roughly **0.0005%** of the pipeline. Hand-tuning it buys
nothing measurable. Meanwhile a default swapchain queues 2–3 frames; at 60 Hz
that is up to 50 ms, more than the sim, the network, and the decode combined.

Which is why the single highest-leverage line in the codebase is in
`crates/pitwall/src/gfx.rs`:

```rust
desired_maximum_frame_latency: 1,   // default is 2–3
```

paired with `PresentMode::Mailbox` (or `Immediate`) in preference to `Fifo`.

**The real reason for Rust here is not throughput — it is the absence of a
garbage collector.** Python decodes a 324-byte packet in ~2 µs, which at 60 Hz
is 0.012% of one core; Python is not too slow to parse telemetry. It is
unusable because a GC pause of 5–15 ms at an unpredictable moment turns a
needle into a stutter, and a driver reads that as a broken instrument.

## Data flow

```
  UDP socket ─┐
              │  [ingest thread, pinned, never allocates after startup]
              │    1. recv_from
              │    2. TIMESTAMP  ← all latency is measured from here
              │    3. record raw bytes (buffered)
              │    4. decode via the protocol registry
              │    5. publish
              │
              ├─→ TripleBuffer<TelemetrySample>  ──→ render thread   (latest-wins)
              └─→ SpscRing<TelemetrySample>      ──→ logger thread  (lossy FIFO)
```

Two structures because gauges and traces want opposite things. A tachometer
does not care about the sample it missed 8 ms ago; a trace graph needs every
sample in order. Neither ever takes a lock — a mutex here would let a 12 µs
network stall become a dropped frame, which is the exact jitter this design
exists to avoid.

The logger thread is the ring's only consumer, and everything that follows from
it — the session log, the rolling pedal chart, the debrief — is described under
[Session recording](#session-recording).

## Protocol identification

Auto-detection is the primary path; the manual switcher is the override. Every
protocol self-identifies:

| Sim | Signature | Confidence |
|---|---|---|
| F1 25 | `m_packetFormat == 2025` at offset 0 | 255 |
| Assetto Corsa | `'a'` in byte 0, plausible RPM | 230 |
| Forza | exact length ∈ {232, 311, 323, 324, 331}, `isRaceOn ∈ {0,1}`, plausible redline | 200 |

Each decoder returns a score, so when two could plausibly claim a datagram the
stronger magic wins. A driver should never have to tell the app what they are
playing — only correct it.

### The Forza 12-byte shift

Forza sends a 232-byte Sled block, optionally followed by a 79-byte Dash block.
**The Horizon titles insert 12 undocumented bytes between them**, shifting
every Dash field:

| Field | FM7 (311 B) | Horizon (324 B) |
|---|---|---|
| speed | 244 | 256 |
| throttle | 303 | 315 |
| gear | 307 | **319** |

This is the origin of the classic "gear reads 255" bug — at 315 in Horizon you
are reading the throttle byte. `dash_base` is derived from the packet length so
one code path handles all four titles.

### The F1 wheel-order trap

Forza and Assetto Corsa send per-wheel arrays as `[FL, FR, RL, RR]`. **The F1
titles send `[RL, RR, FL, FR]`.** Get the permutation wrong and nothing
crashes, no unit test fails, and the app confidently recommends softening the
rear bar on a car that understeers.

`crates/telemetry-analysis/tests/wire_to_advice.rs` guards this: the same
synthetic car, encoded in each sim's native wire format, must produce the same
setup verdict.

### Assetto Corsa is request/response

AC sends nothing until asked: a 12-byte handshake, a 408-byte identity reply,
then a subscribe datagram. The ingest loop drives that state machine and
retries while nothing is arriving.

**ACC is a different problem.** Its UDP Broadcasting API carries session and
timing data, not physics. The real ACC feed is a shared-memory mapping at
~333 Hz, readable only on the machine running the sim. A remote rig therefore
needs a small forwarder process on the sim PC. That bridge is not implemented;
the `SimId::Acc` variant exists so adding it does not disturb the registry.

## Rendering

Three passes:

1. **Backdrop** — fullscreen triangle into an offscreen `Rgba16Float` target.
2. **Blur** — dual Kawase, 5 levels down and back up.
3. **Composite** — every UI element as one instanced draw.

**Dual Kawase, not Gaussian.** A Gaussian wide enough to look like real frosted
glass needs a large kernel at full resolution; dual Kawase ping-pongs between
progressively smaller mips with a fixed 5/8-tap kernel, so cost falls
geometrically. This is what desktop compositors ship for exactly this effect.

**`Rgba16Float`, not `Rgba8`.** The blur averages many samples of a dark
gradient; 8-bit banding survives dithering once it has been through five
passes.

**Everything is an SDF.** Panels, arcs, bars, and glyphs are one instanced quad
each, branching on a shape tag — no meshes, no pre-rendered images. The
distance field is reused for the lighting: `sd_round_box` gives distance-to-edge
for free, which drives the rim highlight, the inner shadow, and the refraction
offset. That reuse is why the glass costs almost nothing beyond the fill.

The glass is four ingredients: a wide-blurred backdrop sampled through a
refraction offset derived from the SDF gradient; a Fresnel rim lit from the
upper left; a soft inner shadow; and blue-noise dither before output.

## Typography

Text is rendered from a **signed distance field**, not a bitmap.

The first version stored glyphs as a 5×7 bitmap in an 8×8 cell and sampled it
with a linear filter. At label sizes that is roughly 1:1 and looks fine. The
gear indicator is drawn at `tach_radius * 0.62` — about 145 px on a 900 px
window and 350 px at 4K. Magnifying a 7-pixel-tall bitmap by 20–50× does not
produce a large glyph; it produces thirty-five fat gradient blobs. No DPI
correction, sampler mode, or canvas-scaling fix addresses that, because the
information is not in the source.

A bitmap stores *coverage* — a step function, which is exactly what bilinear
interpolation cannot reconstruct. An SDF stores *distance to the edge*, a
smooth field that interpolates correctly. The shader recovers a sharp edge at
any magnification with one `smoothstep` around the 0.5 iso-contour:

```wgsl
let d = glyph_sdf(idx, uv);
let w = max(fwidth(d), 1e-5);      // exactly one pixel, at any scale
let fill = smoothstep(0.5 - w, 0.5 + w, d);
```

`fwidth` is what makes this scale-free: it is the screen-space rate of change
of the distance, so the antialiasing window is one pixel wide whether the glyph
is 10 px or 500 px. One expression, no LOD bias, no per-size assets.

### The field is exact, not rasterised

Typical SDF font pipelines rasterise glyphs at high resolution and run a
distance transform, baking in the rasteriser's error. Here the source is not an
image: each glyph is a set of **capsules** joining adjacent lit cells in the
5×7 grid, and the signed distance to a union of capsules has a closed form.
Every texel gets an analytically exact distance.

Two things had to be right, and both failed loudly when they weren't:

- **Every adjacency must be linked, not just diagonals.** Discs that merely
  overlap do not make a stroke. Two discs 1.0 apart with r = 0.52 are 1.04
  units wide at each centre and 0.29 at the waist between them — a 3.6× pulse
  along every stroke. Invisible at label sizes; a chain of beads with scalloped
  edges on a 200 px numeral. Capsules between adjacent centres have constant
  width by construction.
- **Connectivity must be decoupled from weight.** Sizing the pen so diagonal
  neighbours merge (r ≥ 0.707) gives a 1.48-unit stroke and leaves the counter
  of `0` just 0.52 units of daylight. Linking adjacencies explicitly lets the
  pen be thin — 0.36 — with open counters.

### Sharp corners

A single-channel SDF rounds corners tighter than about one texel. That is the
known limitation MSDF exists to solve, and it does not matter here because
these letterforms are built from capsules and are round by construction. If the
font is ever replaced with a real typeface with mitred corners, MSDF is the
upgrade path: same shader, three channels and a `median()`.

## The ignition sequence

The app opens on a standby screen and transitions into the dashboard on any
key. The whole sequence is a function of one scalar running 0 → 1 over 1.9 s,
which keeps it reproducible and scrubbable — `--screenshot --ignition-at 0.35`
renders any instant of it.

Three things happen on that scalar:

- **The standby screen recedes** rather than fading: it lifts and scales up
  slightly as it goes, and it is gone by 28% so the two screens never compete
  for attention.
- **Panels arrive staggered**, each rising and scaling into place on its own
  delay. Staggering is what makes a transition read as choreographed instead of
  as everything fading in at once.
- **The tachometer sweeps to the stop and back.** A real car does this when you
  turn the key — a self-test that became theatre. It is the single cue that
  makes the boot read as a machine starting rather than a page loading.

The stagger is implemented as a transform on the draw list rather than threaded
through every drawing helper: `set_reveal(alpha, offset, scale, pivot)`, draw
the panel normally, `clear_reveal()`. Instances at zero alpha are dropped at
the push site, so panels that have not arrived never reach the instance buffer.

## Hover

Hit-testing and drawing read panel rectangles from the same [`Layout`], because
a layout computed twice is a layout that drifts. The tacho gets a radial test —
hovering the empty corner of its bounding box should not light it up.

Hover is a **spring**, not a boolean, and slightly under-damped (ζ = 0.72): the
point of a hover state is the motion into and out of it, and that hint of
overshoot is what makes a surface feel physical instead of switched.

It costs no extra draw call and no extra pass. Hover is one more channel on the
glass instance, and the shader spends it on parameters it already had: a wider
and brighter Fresnel rim, deeper refraction, an interior bloom, and a ~0.7%
growth of the panel. Large enough to register as the surface leaning forward,
small enough that it never reads as the layout shifting.

## Animation

Two models, and the distinction is most of what makes a cluster feel real.

**`Smoothed`** — exponential smoothing with a real time constant,
`α = 1 − e^(−dt/τ)`. The everywhere-default `value += (target − value) * 0.2` is
frame-rate dependent: at 240 Hz it converges four times faster than at 60.

**`Spring`** — critically damped second-order system, integrated
**analytically**:

```
x(t) = target + (A + Bt)·e^(−ωt)     A = x₀ − target,  B = v₀ + ωA
```

Not numerically. A stiff needle (ω = 300 rad/s for a 20 ms settle) integrated
with semi-implicit Euler is stable only while ω·dt < 2, i.e. dt < 6.7 ms. One
long frame — a shader recompile, a window drag — and the needle does not lag,
it diverges to 1e11 within four steps. The closed form is exact at any timestep
and degrades correctly: after a two-second stall the needle is simply at its
target, which is what it should be showing anyway.

**`Reconstructor`** — the sim sends at 60 Hz; the display runs at 144 or 240.
This interpolates between the last two samples and extrapolates forward by a
bounded amount, which visually cancels part of the sim's own transport delay.
The cap is deliberate: predicting further than about one source interval turns
a smooth needle jittery exactly when the driver does something abrupt.

## Audio

One rule: **the callback never allocates, locks, or blocks.** Every sound is
synthesised into a fixed f32 buffer at startup; the callback mixes from a
fixed voice pool driven by a lock-free queue. One `malloc` on a realtime thread
is an audible click.

Sounds are synthesised rather than sampled: no assets, no decoder dependency,
no licensing, and they can be tuned by editing a curve. For UI feedback an
envelope over an oscillator is all you need.

## Session recording

Two stages on two threads. The ingest thread exists to empty the socket and
must never do anything that could make it late, so it stamps, decodes,
publishes, and hands every sample straight on over a lock-free ring. The logger
thread owns everything expensive.

```
UDP socket
   │  telemetry-ingest thread        recv -> stamp -> raw capture -> decode
   ├──► TripleBuffer<TelemetrySample>   latest-wins  ──► render: gauges
   └──► SpscRing<TelemetrySample>       lossy FIFO   ──► telemetry-logger
                                                            │
   telemetry-logger thread                                   │
     drain the ring in bounded batches                       │
     ├─ append to SessionLog          (chunked circular arena)
     ├─ fold into the strip decimator (min/max per time bin)
     │      └──► TripleBuffer<StripFrame> ─────────► render: rolling chart
     └─ on `end()`, hand the log over ────────────► session-debrief thread
```

**The render thread never waits.** Per frame: one atomic swap for the newest
`StripFrame`, one `try_lock` to see whether a finished session is available.
`try_lock` and not `lock` — losing the race means collecting the log 16 ms
later, which nobody can perceive, and that is a far better trade than the
possibility of a frame waiting on the logger.

**The arena never moves its contents.** `Vec<TelemetrySample>` doubling at a
million samples is a ~300 MB memcpy and a 2x memory spike, in the middle of a
session. `SessionLog` is a deque of fixed 4096-sample chunks; once
`max_chunks` are live the oldest is recycled to hold the newest samples. So a
long session loses its *beginning*, which for a driving session is the right
thing to lose, and after warm-up the logger does not allocate at all.

**The chart is decimated by envelope, not by sampling.** Each column holds the
min and max of its ~31 ms time bin. One sample per column would alias: a 25 ms
brake stab would vanish or flicker depending on where it fell. Min/max means a
transient can be widened to one column but never lost — the same guarantee a
professional trace gives you, and the reason the chart can be trusted to show
pedal overlap. A bin with no samples repeats the previous value rather than
dropping to zero, because "the pedal did not move" is the truth and a gap would
read as a fault in the app.

Bin boundaries come from a monotonic clock, and the target index is computed
from elapsed time rather than incremented per wake-up. The logger wakes on a
jittery ~1.5 ms timer; deriving the index keeps the column width exact anyway.

## The debrief

Modal over the cluster rather than a second OS window. A second window means a
second surface, a second swap chain, and a compositor deciding when each of
them presents — and the whole latency argument rests on owning exactly one
present path.

The analysis runs on a worker thread. Even at a few tens of milliseconds it has
no business on the render thread: the modal is animating open while it runs,
and a stutter in that animation is exactly what makes software feel cheap. The
window opens in a working state and fills in when the result lands.

**Traces are aligned by distance, not by time.** Two laps plotted against time
diverge immediately and cannot be compared; against distance, the same corner
is at the same x on every lap. Where the simulator publishes lap distance it is
used directly; otherwise distance is integrated from speed, and the window says
which of the two it did.

**Pedal channels decimate by peak, motion channels by mean.** Speed and
longitudinal g are smooth, so averaging a 13 m bin is faithful. Throttle and
brake are effectively switched signals where a 30 ms stab is the whole story,
and averaging one into a bin erases it.

Sector splits are three equal distances, not the circuit's real markers: no UDP
protocol here publishes those consistently, and equal thirds at least compare
like-for-like between laps of the same session.

Maximum braking force is qualified — measured only while the brake is applied
and the car is above 5 m/s. Unqualified, the figure reports the impact at the
end of the session rather than the driver's best stop.

## The analysis layer

A heuristic vehicle dynamics engine: deterministic arithmetic, no network, no
fitted parameters. The same lap always yields the same verdict, and every
verdict traces back to the corner that produced it.

- **Corner segmentation** from smoothed lateral load, with hysteresis so a
  mid-corner dip doesn't split one corner into two.
- **Driver fingerprint**: brake onset rate, trail-brake index
  (`∫ brake·|lat g| dt` over entry), coast time, time-to-throttle, steering
  reversal rate.
- **Balance** = mean rear-minus-front slip angle, **weighted by lateral load**.
  The weighting is required, not a refinement: during straight-line braking
  both axles sit near 0° and the balance is structurally zero, so a plain mean
  averages a real handling trait against meaningless zeros and halves it —
  enough to report a car as neutral that the driver can plainly feel is not.
- **Gearing**: RPM histogram, time on the limiter, peak fraction of redline in
  the highest used gear. The most reliably correct recommendation the app can
  make, because it barely involves the driver.

Coast time is reported as a **driving** note, never a setup change. When a
driver is losing three tenths to the pedal transition, changing the
differential will not fix it, and pretending otherwise is how setup advice
loses credibility.

## What is not built

Stated plainly rather than left to be discovered:

- **The ACC shared-memory bridge.** Design sketched above; not implemented.
- **Live sim pinning from the UI.** `Tab`/`P` drive the replay registry; the
  live ingest thread owns its own registry and needs a control channel.
- **Track maps and delta timing.** The capture format and the analysis layer
  support both; nothing draws them yet.
- **Lap-over-lap overlay.** The debrief aligns the fastest complete lap by
  distance, which is the hard half of the problem, but only draws one lap. A
  second trace per plot is the obvious next step.
- **Dashboard hover is visual only.** Panels light up but nothing on the
  cluster is clickable; there is no drill-down, no panel focus, no reordering.
  The debrief's close control is the only interactive element in the app.
- **Sector splits are equal distances, not the circuit's own sectors.** They
  compare laps within a session correctly and cannot be compared against a
  leaderboard.
- **No disk format for sessions.** `.pwtl` captures the wire bytes, and the
  debrief is computed from memory. A session that has been ended and closed is
  gone unless `--record` was also on.
- **A CoreAudio underrun is still reported once at stream start** on macOS.
  cpal reports the device's supported buffer range as unknown there, so the
  explicit 256-frame request falls through to the device default. It is a
  one-time transient at stream start, not an ongoing dropout, and the error
  callback now reports it once instead of flooding stderr from a realtime
  thread.
