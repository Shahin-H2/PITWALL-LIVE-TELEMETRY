//! Interaction sound design.
//!
//! # The one rule
//!
//! **The audio callback must never allocate, lock, or block.** It runs on a
//! realtime thread with a hard deadline measured in single-digit milliseconds;
//! a `Mutex` that is contended once, or a `Vec` that reallocates once, is an
//! audible click. Every sound here is rendered into a fixed buffer at startup,
//! and the callback does nothing but mix pre-computed f32 samples out of a
//! fixed-size voice pool, driven by a lock-free queue.
//!
//! # Why the sounds are synthesised rather than loaded
//!
//! No asset files, no decoder dependency, no licensing, and — the real reason —
//! they can be tuned by editing a curve instead of opening a DAW. Every sound
//! is an envelope over an oscillator, which for UI feedback is all you need:
//! a shift click is a short burst with a fast attack and a 40 ms decay, and
//! nothing sampled would be better.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub const SAMPLE_RATE: u32 = 48_000;
/// How many sounds can overlap. Beyond this the oldest voice is stolen.
const VOICES: usize = 16;
/// Command queue depth. Generous — a burst of UI events must never block.
const QUEUE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sfx {
    /// Gear change — short, dry, percussive.
    Shift,
    /// Rev limiter — a harder, brighter tick.
    Limiter,
    /// UI selection.
    Click,
    /// Sim connected.
    Connect,
    /// Sim lost.
    Disconnect,
}

impl Sfx {
    const ALL: [Sfx; 5] = [Sfx::Shift, Sfx::Limiter, Sfx::Click, Sfx::Connect, Sfx::Disconnect];

    fn index(self) -> usize {
        match self {
            Sfx::Shift => 0,
            Sfx::Limiter => 1,
            Sfx::Click => 2,
            Sfx::Connect => 3,
            Sfx::Disconnect => 4,
        }
    }
}

/// Render one sound into an f32 buffer.
fn synth(kind: Sfx) -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    let (dur, base) = match kind {
        Sfx::Shift => (0.055, 880.0),
        Sfx::Limiter => (0.035, 1450.0),
        Sfx::Click => (0.030, 1200.0),
        Sfx::Connect => (0.220, 520.0),
        Sfx::Disconnect => (0.260, 320.0),
    };
    let n = (sr * dur) as usize;
    let mut out = Vec::with_capacity(n);

    // A tiny deterministic LCG for the noise component. Deterministic so the
    // sound is byte-identical every run — useful when A/B-ing the design.
    let mut rng: u32 = 0x5EED_1337;
    let mut next_noise = move || -> f32 {
        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((rng >> 8) as f32 / 8_388_608.0) - 1.0
    };

    for i in 0..n {
        let t = i as f32 / sr;
        let p = t / dur; // 0..1 through the sound

        // Attack in ~2 ms, then exponential decay. The fast attack is what
        // makes a UI sound feel responsive rather than soft.
        let attack = (t / 0.002).min(1.0);
        let env = attack * (-p * 5.0).exp();

        let s = match kind {
            Sfx::Shift => {
                // Pitch drops through the click — a mechanical "chunk".
                let f = base * (1.0 - p * 0.35);
                let tone = (t * f * std::f32::consts::TAU).sin();
                tone * 0.55 + next_noise() * 0.30 * (1.0 - p)
            }
            Sfx::Limiter => {
                let tone = (t * base * std::f32::consts::TAU).sin();
                let harm = (t * base * 2.0 * std::f32::consts::TAU).sin() * 0.4;
                (tone + harm) * 0.5
            }
            Sfx::Click => {
                let tone = (t * base * std::f32::consts::TAU).sin();
                tone * 0.35 + next_noise() * 0.20 * (1.0 - p)
            }
            Sfx::Connect => {
                // Rising two-note interval: reads as "on".
                let f = base * if p < 0.5 { 1.0 } else { 1.5 };
                (t * f * std::f32::consts::TAU).sin() * 0.4
            }
            Sfx::Disconnect => {
                // Falling: reads as "off". Same construction, inverted.
                let f = base * if p < 0.5 { 1.5 } else { 1.0 };
                (t * f * std::f32::consts::TAU).sin() * 0.4
            }
        };
        out.push(s * env);
    }
    out
}

/// Lock-free SPSC command queue. The UI thread pushes; the audio callback pops.
struct CmdQueue {
    slots: [AtomicUsize; QUEUE],
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl CmdQueue {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicUsize::new(usize::MAX)),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    fn push(&self, v: usize) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= QUEUE {
            return; // full: drop the sound rather than stall the UI thread
        }
        self.slots[head % QUEUE].store(v, Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self) -> Option<usize> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        let v = self.slots[tail % QUEUE].load(Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(v)
    }
}

#[derive(Clone, Copy, Default)]
struct Voice {
    bank: usize,
    pos: usize,
    active: bool,
}

pub struct Audio {
    queue: Arc<CmdQueue>,
    // Held so the stream stays alive; dropping it stops playback.
    _stream: Option<cpal::Stream>,
    pub enabled: bool,
}

impl Audio {
    /// Start the output stream. Returns a silent, inert `Audio` if no device
    /// is available — a missing sound card must never stop the app.
    pub fn new(volume: f32) -> Self {
        let queue = Arc::new(CmdQueue::new());
        let banks: Arc<Vec<Vec<f32>>> = Arc::new(Sfx::ALL.iter().map(|s| synth(*s)).collect());

        let stream = Self::build_stream(Arc::clone(&queue), Arc::clone(&banks), volume);
        match &stream {
            Some(_) => eprintln!("[audio] output stream running at {SAMPLE_RATE} Hz"),
            None => eprintln!("[audio] no output device; sound disabled"),
        }

        Self {
            queue,
            enabled: stream.is_some(),
            _stream: stream,
        }
    }

    fn build_stream(
        queue: Arc<CmdQueue>,
        banks: Arc<Vec<Vec<f32>>>,
        volume: f32,
    ) -> Option<cpal::Stream> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let default_cfg = device.default_output_config().ok()?;
        let channels = default_cfg.channels() as usize;
        let sample_format = default_cfg.sample_format();
        let mut config: cpal::StreamConfig = default_cfg.into();

        // Ask for a small, explicit buffer.
        //
        // The device default on macOS can be large enough that the first
        // callback arrives late and CoreAudio reports an underrun before we
        // have played a single sample. Requesting ~256 frames (5.3 ms at
        // 48 kHz) both fixes that and is what we want anyway: interaction
        // sound that lands more than ~10 ms after the event stops reading as
        // a response to it. Clamped into whatever the device will accept,
        // because an out-of-range request fails the whole stream build.
        const TARGET_FRAMES: u32 = 256;
        config.buffer_size = match device.default_output_config().ok().map(|c| c.buffer_size().clone()) {
            Some(cpal::SupportedBufferSize::Range { min, max }) => {
                cpal::BufferSize::Fixed(TARGET_FRAMES.clamp(min, max))
            }
            // Unknown range: leave the device to choose rather than guess.
            _ => cpal::BufferSize::Default,
        };

        // The voice pool lives in the closure, so it is owned solely by the
        // audio thread. No sharing, therefore no synchronisation needed on it.
        let mut voices = [Voice::default(); VOICES];
        let mut next_voice = 0usize;

        let mut fill = move |out: &mut [f32]| {
            // Drain new commands first so a sound triggered this buffer starts
            // in this buffer, not the next one.
            while let Some(bank) = queue.pop() {
                if bank >= banks.len() {
                    continue;
                }
                // Round-robin voice stealing: always succeeds, never allocates.
                voices[next_voice] = Voice { bank, pos: 0, active: true };
                next_voice = (next_voice + 1) % VOICES;
            }

            for frame in out.chunks_mut(channels) {
                let mut mix = 0.0f32;
                for v in voices.iter_mut() {
                    if !v.active {
                        continue;
                    }
                    let bank = &banks[v.bank];
                    if v.pos >= bank.len() {
                        v.active = false;
                        continue;
                    }
                    mix += bank[v.pos];
                    v.pos += 1;
                }
                // Soft clip. `tanh` is smooth, so simultaneous sounds
                // compress rather than crackle.
                let s = (mix * volume).tanh();
                for c in frame.iter_mut() {
                    *c = s;
                }
            }
        };

        // CoreAudio reports a spurious underrun on some devices as the stream
        // spins up. Report the first one and then stay quiet, rather than
        // flooding stderr from a realtime thread.
        let mut reported = false;
        let err = move |e| {
            if !reported {
                reported = true;
                eprintln!("[audio] stream error (further errors suppressed): {e}");
            }
        };

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device
                .build_output_stream(
                    config,
                    move |data: &mut [f32], _| fill(data),
                    err,
                    None,
                )
                .ok()?,
            other => {
                eprintln!("[audio] unsupported sample format {other:?}; sound disabled");
                return None;
            }
        };
        stream.play().ok()?;
        Some(stream)
    }

    /// An inert instance, for `--no-audio` and for tests. Every `play` call
    /// becomes a no-op, so callers never need to branch on whether sound is on.
    pub fn disabled() -> Self {
        Self {
            queue: Arc::new(CmdQueue::new()),
            _stream: None,
            enabled: false,
        }
    }

    /// Trigger a sound. Safe to call from the render thread every frame — it
    /// is a single atomic store on the fast path.
    pub fn play(&self, sfx: Sfx) {
        if self.enabled {
            self.queue.push(sfx.index());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_renders_non_silent_audio() {
        for s in Sfx::ALL {
            let buf = synth(s);
            assert!(!buf.is_empty(), "{s:?} produced no samples");
            let peak = buf.iter().fold(0.0f32, |a, b| a.max(b.abs()));
            assert!(peak > 0.05, "{s:?} is effectively silent (peak {peak})");
            assert!(peak <= 1.0, "{s:?} clips before the mixer (peak {peak})");
            assert!(buf.iter().all(|v| v.is_finite()), "{s:?} contains NaN");
        }
    }

    #[test]
    fn envelopes_start_and_end_near_zero() {
        // A sound that starts or ends on a non-zero sample produces a click.
        for s in Sfx::ALL {
            let buf = synth(s);
            assert!(buf[0].abs() < 0.05, "{s:?} starts at {}", buf[0]);
            let tail = buf[buf.len() - 1].abs();
            assert!(tail < 0.05, "{s:?} ends at {tail}");
        }
    }

    #[test]
    fn queue_is_lossy_not_blocking_when_full() {
        let q = CmdQueue::new();
        for _ in 0..QUEUE * 2 {
            q.push(1); // must not hang or panic
        }
        let mut got = 0;
        while q.pop().is_some() {
            got += 1;
        }
        assert_eq!(got, QUEUE, "queue should hold exactly its capacity");
    }

    #[test]
    fn queue_preserves_order() {
        let q = CmdQueue::new();
        for i in 0..5 {
            q.push(i);
        }
        for i in 0..5 {
            assert_eq!(q.pop(), Some(i));
        }
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn synth_is_deterministic() {
        // The noise generator is seeded, so two renders must be identical.
        assert_eq!(synth(Sfx::Shift), synth(Sfx::Shift));
    }
}
