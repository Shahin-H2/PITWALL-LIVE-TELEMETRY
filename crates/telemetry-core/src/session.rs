//! Session recording and the live strip-chart decimator.
//!
//! This is the second stage of the ingest pipeline. The first stage (the
//! `telemetry-ingest` thread) exists to empty the socket; it must never do
//! anything that could make it late. So it does the minimum — stamp, decode,
//! publish — and hands every decoded sample straight on to *this* stage over a
//! lock-free ring.
//!
//! ```text
//!   UDP socket
//!      │  telemetry-ingest thread          recv → stamp → decode
//!      ├──► TripleBuffer<TelemetrySample>  latest-wins  ──► render: gauges
//!      └──► SpscRing<TelemetrySample>      lossy FIFO   ──► telemetry-logger
//!                                                             │
//!   telemetry-logger thread                                    │
//!     drain the ring in bounded batches                        │
//!     ├─ append to SessionLog          (chunked circular arena)
//!     ├─ fold into the strip decimator (min/max per time bin)
//!     │     └──► TripleBuffer<StripFrame> ──────────► render: rolling chart
//!     └─ on `end()`, hand the whole log over ───────► debrief thread
//! ```
//!
//! Three properties are load-bearing, and each one is a deliberate choice
//! rather than a side effect:
//!
//! * **The render thread never waits.** Per frame it performs one atomic swap
//!   to pick up the newest [`StripFrame`] and, at most, one `try_lock` to see
//!   whether a finished session is available. Neither can block, so no amount
//!   of logging work can turn into a dropped frame.
//!
//! * **Logging never applies backpressure to the socket.** The ring between
//!   the two threads drops on overflow and counts what it dropped. A stalled
//!   logger degrades the *log*, never the live gauges and never the socket.
//!
//! * **The arena never moves its contents.** A plain `Vec<TelemetrySample>`
//!   doubling at a million samples is a ~300 MB memcpy and a 2× memory spike,
//!   in the middle of a session. [`SessionLog`] grows one fixed chunk at a
//!   time and recycles the oldest chunk once it is full, so after warm-up it
//!   does not allocate at all.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::ring::{self, Consumer, Producer, SpscRing};
use crate::sample::TelemetrySample;

// ============================================================================
// The session log
// ============================================================================

/// Samples per arena chunk. At 300 Hz this is ~13.6 s of driving, and about
/// 1.2 MB, which is small enough to allocate mid-session without a visible
/// pause and large enough that allocation happens rarely.
pub const LOG_CHUNK: usize = 4096;

/// An append-only session recording.
///
/// Chunked rather than contiguous, and circular at chunk granularity: once
/// `max_chunks` are live, the oldest chunk is recycled to hold the newest
/// samples. A session that outruns its capacity therefore loses its *start*,
/// which for a driving session is the right thing to lose — the interesting
/// laps are the recent ones.
pub struct SessionLog {
    /// Oldest chunk first. Every chunk but the last is completely full.
    chunks: VecDeque<Box<[TelemetrySample]>>,
    /// Samples used in the newest chunk.
    tail_used: usize,
    max_chunks: usize,
    /// Samples discarded from the front because the arena wrapped.
    overwritten: u64,
}

impl SessionLog {
    /// Capacity is rounded up to a whole number of chunks, minimum one.
    pub fn with_capacity(max_samples: usize) -> Self {
        let max_chunks = max_samples.div_ceil(LOG_CHUNK).max(1);
        Self {
            chunks: VecDeque::with_capacity(max_chunks.min(64)),
            tail_used: 0,
            max_chunks,
            overwritten: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, s: TelemetrySample) {
        if self.tail_used == LOG_CHUNK || self.chunks.is_empty() {
            if self.chunks.len() >= self.max_chunks {
                // Recycle the oldest chunk instead of allocating. This is the
                // steady state of a long session: no allocator traffic at all.
                let mut recycled = self.chunks.pop_front().expect("max_chunks >= 1");
                recycled[0] = s;
                self.chunks.push_back(recycled);
                self.tail_used = 1;
                self.overwritten += LOG_CHUNK as u64;
                return;
            }
            let mut fresh = vec![TelemetrySample::default(); LOG_CHUNK].into_boxed_slice();
            fresh[0] = s;
            self.chunks.push_back(fresh);
            self.tail_used = 1;
            return;
        }
        let last = self.chunks.len() - 1;
        self.chunks[last][self.tail_used] = s;
        self.tail_used += 1;
    }

    pub fn len(&self) -> usize {
        match self.chunks.len() {
            0 => 0,
            n => (n - 1) * LOG_CHUNK + self.tail_used,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many samples fell out of the front of the arena.
    pub fn overwritten(&self) -> u64 {
        self.overwritten
    }

    /// The live samples, oldest first, as contiguous runs.
    pub fn runs(&self) -> impl Iterator<Item = &[TelemetrySample]> {
        let last = self.chunks.len().saturating_sub(1);
        let used = self.tail_used;
        self.chunks.iter().enumerate().map(move |(i, c)| {
            if i == last {
                &c[..used]
            } else {
                &c[..]
            }
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &TelemetrySample> {
        self.runs().flatten()
    }

    /// Flatten into one contiguous slice for the analysis pass.
    ///
    /// This copies, deliberately and exactly once, on the thread that is about
    /// to spend far longer analysing it. Keeping the live arena chunked is
    /// worth one linear copy at the end of a session.
    pub fn to_vec(&self) -> Vec<TelemetrySample> {
        let mut out = Vec::with_capacity(self.len());
        for run in self.runs() {
            out.extend_from_slice(run);
        }
        out
    }

    pub fn clear(&mut self) {
        // Keep the allocated chunks; a new session will refill them.
        while self.chunks.len() > 1 {
            self.chunks.pop_front();
        }
        self.tail_used = 0;
        self.overwritten = 0;
    }
}

// ============================================================================
// The rolling strip chart
// ============================================================================

/// Columns in the live strip chart. One column per time bin.
pub const STRIP_COLUMNS: usize = 256;

/// One decimated column: the envelope of a channel over its time bin.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StripColumn {
    pub thr_min: f32,
    pub thr_max: f32,
    pub brk_min: f32,
    pub brk_max: f32,
    /// Fraction of the bin during which both pedals were applied together.
    pub overlap: f32,
}

/// A snapshot of the last [`StripFrame::window_s`] seconds of pedal input.
///
/// **Decimated by min/max, not by sampling.** Picking one sample per column
/// would alias: a 25 ms brake stab inside a 31 ms bin would vanish or flicker
/// depending on phase. Keeping the *envelope* of each bin means a transient can
/// never be lost, only widened to one column — the same guarantee a MoTeC trace
/// gives you, and the reason the chart can be trusted to show pedal overlap.
///
/// Fixed size and `Copy` so it moves through a triple buffer with one memcpy
/// and no allocation. ~5 KB at 50 Hz is 250 KB/s of bandwidth, against the
/// alternative of the render thread walking a shared history buffer under a
/// lock once a frame.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct StripFrame {
    thr_min: [f32; STRIP_COLUMNS],
    thr_max: [f32; STRIP_COLUMNS],
    brk_min: [f32; STRIP_COLUMNS],
    brk_max: [f32; STRIP_COLUMNS],
    overlap: [f32; STRIP_COLUMNS],
    /// Index of the newest closed column.
    head: u32,
    /// How many columns hold data, at most [`STRIP_COLUMNS`].
    filled: u32,
    /// Seconds of history the full chart spans.
    pub window_s: f32,
    /// Seconds per column.
    pub bin_s: f32,
    /// Samples folded into the columns currently on screen. Zero means the sim
    /// has gone quiet and the trace is being held, not measured.
    pub samples: u32,
    /// Columns in this window that contained pedal overlap.
    pub overlap_columns: u32,
}

impl Default for StripFrame {
    // `[f32; 256]` has no `Default` impl (the standard library only provides
    // one up to 32 elements), so this is written out rather than derived.
    fn default() -> Self {
        Self {
            thr_min: [0.0; STRIP_COLUMNS],
            thr_max: [0.0; STRIP_COLUMNS],
            brk_min: [0.0; STRIP_COLUMNS],
            brk_max: [0.0; STRIP_COLUMNS],
            overlap: [0.0; STRIP_COLUMNS],
            head: (STRIP_COLUMNS - 1) as u32,
            filled: 0,
            window_s: 0.0,
            bin_s: 0.0,
            samples: 0,
            overlap_columns: 0,
        }
    }
}

impl StripFrame {
    /// Columns in a full window.
    pub const CAPACITY: usize = STRIP_COLUMNS;

    /// Number of valid columns.
    #[inline]
    pub fn len(&self) -> usize {
        self.filled as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }

    /// Column `i`, where 0 is the oldest and `len() - 1` the newest.
    ///
    /// The storage is a ring; this is the only place that knows it.
    pub fn column(&self, i: usize) -> Option<StripColumn> {
        let filled = self.filled as usize;
        if i >= filled {
            return None;
        }
        let oldest = (self.head as usize + STRIP_COLUMNS - filled + 1) % STRIP_COLUMNS;
        let k = (oldest + i) % STRIP_COLUMNS;
        Some(StripColumn {
            thr_min: self.thr_min[k],
            thr_max: self.thr_max[k],
            brk_min: self.brk_min[k],
            brk_max: self.brk_max[k],
            overlap: self.overlap[k],
        })
    }

    /// Iterate the window oldest-first.
    pub fn columns(&self) -> impl Iterator<Item = StripColumn> + '_ {
        (0..self.len()).filter_map(move |i| self.column(i))
    }
}

/// Folds a sample stream into [`StripFrame`] columns.
///
/// Bin boundaries come from a monotonic clock, not from the arrival of samples:
/// the chart must scroll at a constant rate whether the sim is sending at
/// 300 Hz, at 20 Hz, or not at all. A bin with no samples repeats the previous
/// value (a zero-order hold), because "the pedal did not move" is the truth,
/// and a gap in the trace would read as a fault in the app.
struct Decimator {
    frame: StripFrame,
    bin_s: f64,
    /// Index of the bin currently being accumulated.
    cur_bin: u64,
    acc: Option<StripColumn>,
    acc_overlap_samples: u32,
    acc_samples: u32,
    /// Samples per stored column, so the window total can be maintained by
    /// adding the new column and subtracting the one it evicts. Recounting all
    /// 256 on every close would work too; this is the same answer for a
    /// constant two operations.
    counts: [u32; STRIP_COLUMNS],
    window_samples: u32,
    hold_thr: f32,
    hold_brk: f32,
}

/// Both pedals above this at once counts as overlap. Low enough to catch a
/// driver dragging the brake into the throttle, high enough to ignore pedal
/// sensor noise and controller deadzone chatter.
const OVERLAP_THRESHOLD: f32 = 0.04;

impl Decimator {
    fn new(window_s: f32) -> Self {
        let bin_s = (window_s as f64 / STRIP_COLUMNS as f64).max(1e-4);
        let mut frame = StripFrame::default();
        frame.window_s = window_s;
        frame.bin_s = bin_s as f32;
        Self {
            frame,
            bin_s,
            cur_bin: 0,
            acc: None,
            acc_overlap_samples: 0,
            acc_samples: 0,
            counts: [0; STRIP_COLUMNS],
            window_samples: 0,
            hold_thr: 0.0,
            hold_brk: 0.0,
        }
    }

    /// Fold one sample into the bin currently open.
    ///
    /// The sample is credited to the open bin rather than to the bin its own
    /// capture timestamp falls in. The difference is bounded by the transport
    /// latency between the two threads — on the order of a millisecond against
    /// a 31 ms bin — so the distinction is below the width of one column.
    #[inline]
    fn accumulate(&mut self, s: &TelemetrySample) {
        let thr = s.throttle.clamp(0.0, 1.0);
        let brk = s.brake.clamp(0.0, 1.0);
        match &mut self.acc {
            Some(a) => {
                a.thr_min = a.thr_min.min(thr);
                a.thr_max = a.thr_max.max(thr);
                a.brk_min = a.brk_min.min(brk);
                a.brk_max = a.brk_max.max(brk);
            }
            None => {
                self.acc = Some(StripColumn {
                    thr_min: thr,
                    thr_max: thr,
                    brk_min: brk,
                    brk_max: brk,
                    overlap: 0.0,
                })
            }
        }
        if thr > OVERLAP_THRESHOLD && brk > OVERLAP_THRESHOLD {
            self.acc_overlap_samples += 1;
        }
        self.acc_samples += 1;
        self.hold_thr = thr;
        self.hold_brk = brk;
    }

    /// Close every bin up to (but not including) `bin`.
    ///
    /// Computing the target index from elapsed time rather than incrementing on
    /// each wake-up keeps the column width exact even though this thread wakes
    /// on a jittery ~1.5 ms timer.
    fn advance_to(&mut self, bin: u64) {
        let mut guard = 0;
        while self.cur_bin < bin {
            self.close_bin();
            self.cur_bin += 1;
            // A long stall (a debugger pause, a suspended laptop) could ask for
            // millions of empty bins. More than a full window of them conveys
            // nothing, so clamp the catch-up and jump the clock.
            guard += 1;
            if guard > STRIP_COLUMNS {
                self.cur_bin = bin;
                break;
            }
        }
    }

    fn close_bin(&mut self) {
        let col = self.acc.take().unwrap_or(StripColumn {
            thr_min: self.hold_thr,
            thr_max: self.hold_thr,
            brk_min: self.hold_brk,
            brk_max: self.hold_brk,
            overlap: 0.0,
        });
        let overlap = if self.acc_samples > 0 {
            self.acc_overlap_samples as f32 / self.acc_samples as f32
        } else {
            0.0
        };

        let head = (self.frame.head as usize + 1) % STRIP_COLUMNS;
        self.frame.head = head as u32;
        // This slot held the column a full window ago; it is leaving the view.
        self.window_samples -= self.counts[head];
        self.counts[head] = self.acc_samples;
        self.window_samples += self.acc_samples;
        self.frame.thr_min[head] = col.thr_min;
        self.frame.thr_max[head] = col.thr_max;
        self.frame.brk_min[head] = col.brk_min;
        self.frame.brk_max[head] = col.brk_max;
        self.frame.overlap[head] = overlap;
        self.frame.filled = (self.frame.filled + 1).min(STRIP_COLUMNS as u32);

        self.frame.samples = self.window_samples;
        self.acc_samples = 0;
        self.acc_overlap_samples = 0;

        // Recount the window's overlap columns from the frame itself, so the
        // figure always describes exactly what is on screen.
        let mut n = 0;
        for i in 0..self.frame.filled as usize {
            if self.frame.overlap[i] > 0.0 {
                n += 1;
            }
        }
        self.frame.overlap_columns = n;
    }
}

// ============================================================================
// Control surface
// ============================================================================

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Draining the ring and driving the live chart, but not keeping anything.
    Idle = 0,
    Recording = 1,
    /// A finished log is waiting to be collected.
    Complete = 2,
}

impl SessionState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => SessionState::Recording,
            2 => SessionState::Complete,
            _ => SessionState::Idle,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            SessionState::Idle => "IDLE",
            SessionState::Recording => "REC",
            SessionState::Complete => "ENDED",
        }
    }
}

const REQ_NONE: u8 = 0;
const REQ_START: u8 = 1;
const REQ_END: u8 = 2;
const REQ_DISCARD: u8 = 3;

/// Shared, lock-free-to-read state between the logger thread and the UI.
#[derive(Default)]
pub struct SessionControl {
    request: AtomicU8,
    state: AtomicU8,
    /// Samples appended to the current (or last) session log.
    pub logged: AtomicU64,
    /// Samples the arena dropped off its front because it was full.
    pub overwritten: AtomicU64,
    /// Samples lost in the ring between ingest and logger. Should stay zero;
    /// if it does not, the logger is not keeping up and that is worth seeing.
    pub lost_in_transit: AtomicU64,
    /// Nanoseconds the last drain-and-fold pass took. The honest measure of
    /// whether logging is cheap.
    pub last_drain_ns: AtomicU64,
    finished: Mutex<Option<SessionLog>>,
}

impl SessionControl {
    pub fn state(&self) -> SessionState {
        SessionState::from_u8(self.state.load(Ordering::Acquire))
    }
}

/// The UI's handle on the logger thread.
pub struct SessionHandle {
    ctl: Arc<SessionControl>,
    strip: Consumer<StripFrame>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub struct SessionConfig {
    /// Hard ceiling on retained samples. At 300 Hz, 540 000 is 30 minutes and
    /// roughly 160 MB. Beyond it the session keeps recording and forgets its
    /// beginning.
    pub max_samples: usize,
    /// Seconds of pedal history the live chart spans.
    pub window_s: f32,
    /// Most samples to move in one wake-up, so a control request is never
    /// stuck behind a large backlog.
    pub drain_batch: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { max_samples: 540_000, window_s: 8.0, drain_batch: 4096 }
    }
}

/// Start the logger thread on `source`.
///
/// `source` must be the ring's **only** consumer: [`SpscRing`] is
/// single-consumer by construction and a second reader would silently steal
/// samples. In practice that means one logger per ingest thread.
pub fn spawn(
    source: Arc<SpscRing<TelemetrySample>>,
    cfg: SessionConfig,
) -> std::io::Result<SessionHandle> {
    let (strip_tx, strip_rx) = ring::triple_buffer::<StripFrame>();
    let ctl = Arc::new(SessionControl::default());
    let stop = Arc::new(AtomicBool::new(false));

    let thread = {
        let ctl = Arc::clone(&ctl);
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("telemetry-logger".into())
            .spawn(move || logger_loop(source, ctl, strip_tx, stop, cfg))?
    };

    Ok(SessionHandle { ctl, strip: strip_rx, stop, thread: Some(thread) })
}

impl SessionHandle {
    pub fn control(&self) -> &Arc<SessionControl> {
        &self.ctl
    }

    pub fn state(&self) -> SessionState {
        self.ctl.state()
    }

    /// Begin (or restart) recording. Discards any uncollected previous log.
    pub fn start(&self) {
        self.ctl.request.store(REQ_START, Ordering::Release);
    }

    /// Stop recording and hand the log over. Poll [`Self::take_log`] for it.
    pub fn end(&self) {
        self.ctl.request.store(REQ_END, Ordering::Release);
    }

    /// Throw the current recording away without producing a debrief.
    pub fn discard(&self) {
        self.ctl.request.store(REQ_DISCARD, Ordering::Release);
    }

    /// The newest strip-chart snapshot. One atomic swap; never blocks.
    pub fn strip(&mut self) -> StripFrame {
        self.strip.latest()
    }

    /// Collect a finished session, if one is ready.
    ///
    /// `try_lock`, never `lock`: this is called from the render thread, and a
    /// frame must never be able to wait on the logger. Losing the race simply
    /// means collecting it 16 ms later, which nobody can perceive.
    pub fn take_log(&self) -> Option<SessionLog> {
        if self.ctl.state() != SessionState::Complete {
            return None;
        }
        match self.ctl.finished.try_lock() {
            Ok(mut g) => g.take(),
            Err(_) => None,
        }
    }

    pub fn logged(&self) -> u64 {
        self.ctl.logged.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn logger_loop(
    source: Arc<SpscRing<TelemetrySample>>,
    ctl: Arc<SessionControl>,
    mut strip_tx: Producer<StripFrame>,
    stop: Arc<AtomicBool>,
    cfg: SessionConfig,
) {
    let mut log = SessionLog::with_capacity(cfg.max_samples);
    let mut dec = Decimator::new(cfg.window_s);
    let epoch = Instant::now();
    // Publish on the very first pass, so the UI has the chart's geometry
    // (window length, column count) before any telemetry arrives.
    let mut last_publish: Option<Instant> = None;
    let mut recording = false;
    let mut ring_dropped_at_start = source.dropped();

    // The chart is published at 60 Hz. A column only closes every ~31 ms, so
    // anything faster would re-send an identical frame.
    let publish_every = Duration::from_micros(16_000);
    // Long enough that the thread is asleep almost all the time, short enough
    // that a 300 Hz feed accumulates at most a sample or two per wake-up.
    let idle_nap = Duration::from_micros(1_500);

    while !stop.load(Ordering::Relaxed) {
        let pass_start = Instant::now();

        // ---- control requests ------------------------------------------------
        match ctl.request.swap(REQ_NONE, Ordering::AcqRel) {
            REQ_START => {
                log.clear();
                ctl.logged.store(0, Ordering::Relaxed);
                ctl.overwritten.store(0, Ordering::Relaxed);
                ring_dropped_at_start = source.dropped();
                ctl.lost_in_transit.store(0, Ordering::Relaxed);
                if let Ok(mut g) = ctl.finished.lock() {
                    *g = None;
                }
                recording = true;
                ctl.state.store(SessionState::Recording as u8, Ordering::Release);
            }
            REQ_END => {
                recording = false;
                if let Ok(mut g) = ctl.finished.lock() {
                    // Move the arena out wholesale. The replacement reuses the
                    // same capacity so the next session does not re-allocate
                    // from nothing.
                    *g = Some(std::mem::replace(
                        &mut log,
                        SessionLog::with_capacity(cfg.max_samples),
                    ));
                }
                ctl.state.store(SessionState::Complete as u8, Ordering::Release);
            }
            REQ_DISCARD => {
                recording = false;
                log.clear();
                ctl.logged.store(0, Ordering::Relaxed);
                if let Ok(mut g) = ctl.finished.lock() {
                    *g = None;
                }
                ctl.state.store(SessionState::Idle as u8, Ordering::Release);
            }
            _ => {}
        }

        // ---- drain ------------------------------------------------------------
        // Always drained, whether or not a session is being recorded: the live
        // chart has to work before the driver has armed anything, and leaving
        // the ring to fill would make the ingest thread count phantom drops.
        let mut moved = 0usize;
        while moved < cfg.drain_batch {
            let Some(s) = source.pop() else { break };
            moved += 1;
            dec.accumulate(&s);
            if recording {
                log.push(s);
            }
        }
        if recording && moved > 0 {
            ctl.logged.store(log.len() as u64, Ordering::Relaxed);
            ctl.overwritten.store(log.overwritten(), Ordering::Relaxed);
        }
        ctl.lost_in_transit.store(
            source.dropped().saturating_sub(ring_dropped_at_start) as u64,
            Ordering::Relaxed,
        );

        // ---- decimate and publish --------------------------------------------
        let bin = (epoch.elapsed().as_secs_f64() / dec.bin_s) as u64;
        dec.advance_to(bin);
        if last_publish.is_none_or(|t| t.elapsed() >= publish_every) {
            strip_tx.publish(dec.frame);
            last_publish = Some(Instant::now());
        }

        ctl.last_drain_ns
            .store(pass_start.elapsed().as_nanos() as u64, Ordering::Relaxed);

        if moved == 0 {
            thread::sleep(idle_nap);
        }
    }

    // On shutdown, leave whatever was recorded collectable rather than
    // dropping it on the floor.
    if recording && !log.is_empty() {
        if let Ok(mut g) = ctl.finished.lock() {
            *g = Some(log);
        }
        ctl.state.store(SessionState::Complete as u8, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::FieldMask;

    fn sample(t_ns: u64, thr: f32, brk: f32) -> TelemetrySample {
        TelemetrySample {
            t_capture_ns: t_ns,
            is_active: true,
            present: FieldMask::THROTTLE | FieldMask::BRAKE,
            throttle: thr,
            brake: brk,
            ..Default::default()
        }
    }

    // ---- the arena --------------------------------------------------------

    #[test]
    fn log_keeps_every_sample_in_order_below_capacity() {
        let mut log = SessionLog::with_capacity(LOG_CHUNK * 3);
        for i in 0..LOG_CHUNK * 2 + 7 {
            log.push(sample(i as u64, 0.0, 0.0));
        }
        assert_eq!(log.len(), LOG_CHUNK * 2 + 7);
        assert_eq!(log.overwritten(), 0);
        for (i, s) in log.iter().enumerate() {
            assert_eq!(s.t_capture_ns, i as u64, "sample {i} out of order");
        }
    }

    #[test]
    fn log_forgets_its_beginning_not_its_end_when_full() {
        // Two chunks of capacity, three chunks of data: the first chunk must
        // be the one that goes.
        let mut log = SessionLog::with_capacity(LOG_CHUNK * 2);
        let total = LOG_CHUNK * 3;
        for i in 0..total {
            log.push(sample(i as u64, 0.0, 0.0));
        }
        assert_eq!(log.overwritten(), LOG_CHUNK as u64);
        let v = log.to_vec();
        assert_eq!(v.len(), LOG_CHUNK * 2);
        assert_eq!(
            v.last().unwrap().t_capture_ns,
            (total - 1) as u64,
            "the newest sample was lost, which is exactly backwards"
        );
        assert_eq!(v[0].t_capture_ns, LOG_CHUNK as u64);
        for w in v.windows(2) {
            assert_eq!(w[1].t_capture_ns, w[0].t_capture_ns + 1, "wrap broke ordering");
        }
    }

    #[test]
    fn a_full_arena_stops_allocating() {
        // Once wrapped, pushing must recycle chunks rather than grow. Checked
        // through the public surface: the chunk count is bounded by capacity.
        let mut log = SessionLog::with_capacity(LOG_CHUNK * 2);
        for i in 0..LOG_CHUNK * 20 {
            log.push(sample(i as u64, 0.0, 0.0));
        }
        assert_eq!(log.runs().count(), 2, "the arena grew past its capacity");
        assert_eq!(log.len(), LOG_CHUNK * 2);
    }

    #[test]
    fn to_vec_matches_iter() {
        let mut log = SessionLog::with_capacity(LOG_CHUNK * 2);
        for i in 0..LOG_CHUNK + 31 {
            log.push(sample(i as u64, 0.0, 0.0));
        }
        let a: Vec<u64> = log.iter().map(|s| s.t_capture_ns).collect();
        let b: Vec<u64> = log.to_vec().iter().map(|s| s.t_capture_ns).collect();
        assert_eq!(a, b);
    }

    // ---- decimation -------------------------------------------------------

    #[test]
    fn strip_columns_read_back_oldest_first() {
        let mut dec = Decimator::new(8.0);
        for i in 0..5 {
            dec.accumulate(&sample(0, i as f32 * 0.1, 0.0));
            dec.advance_to(i as u64 + 1);
        }
        let f = dec.frame;
        assert_eq!(f.len(), 5);
        let vals: Vec<f32> = f.columns().map(|c| c.thr_max).collect();
        assert_eq!(vals, vec![0.0, 0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn the_ring_wraps_without_reordering() {
        let mut dec = Decimator::new(8.0);
        let n = STRIP_COLUMNS + 40;
        for i in 0..n {
            dec.accumulate(&sample(0, (i % 100) as f32 / 100.0, 0.0));
            dec.advance_to(i as u64 + 1);
        }
        let f = dec.frame;
        assert_eq!(f.len(), STRIP_COLUMNS, "the window should be exactly full");
        let newest = f.column(f.len() - 1).unwrap();
        assert!(
            (newest.thr_max - ((n - 1) % 100) as f32 / 100.0).abs() < 1e-6,
            "the last column is not the newest sample"
        );
        let oldest = f.column(0).unwrap();
        assert!(
            (oldest.thr_max - ((n - STRIP_COLUMNS) % 100) as f32 / 100.0).abs() < 1e-6,
            "the first column is not the oldest retained sample"
        );
    }

    #[test]
    fn a_transient_inside_one_bin_survives_decimation() {
        // This is the whole reason for min/max rather than point sampling: a
        // brake stab shorter than a column must still be visible.
        let mut dec = Decimator::new(8.0);
        dec.accumulate(&sample(0, 1.0, 0.0));
        dec.accumulate(&sample(0, 1.0, 0.85)); // the stab
        dec.accumulate(&sample(0, 1.0, 0.0));
        dec.advance_to(1);
        let c = dec.frame.column(0).unwrap();
        assert!((c.brk_max - 0.85).abs() < 1e-6, "the stab was decimated away");
        assert!((c.brk_min - 0.0).abs() < 1e-6, "the envelope lost its floor");
    }

    #[test]
    fn empty_bins_hold_the_last_value_rather_than_dropping_to_zero() {
        let mut dec = Decimator::new(8.0);
        dec.accumulate(&sample(0, 0.7, 0.0));
        dec.advance_to(1);
        // Five bins with no samples at all: the sim went quiet.
        dec.advance_to(6);
        for i in 1..6 {
            let c = dec.frame.column(i).unwrap();
            assert!(
                (c.thr_max - 0.7).abs() < 1e-6,
                "bin {i} dropped to zero instead of holding 0.7"
            );
        }
    }

    #[test]
    fn overlap_is_measured_as_a_fraction_of_the_bin() {
        let mut dec = Decimator::new(8.0);
        dec.accumulate(&sample(0, 0.5, 0.5)); // overlapping
        dec.accumulate(&sample(0, 0.5, 0.0));
        dec.accumulate(&sample(0, 0.5, 0.0));
        dec.accumulate(&sample(0, 0.5, 0.0));
        dec.advance_to(1);
        let c = dec.frame.column(0).unwrap();
        assert!((c.overlap - 0.25).abs() < 1e-6, "got {}", c.overlap);
        assert_eq!(dec.frame.overlap_columns, 1);
    }

    #[test]
    fn tiny_simultaneous_readings_are_not_called_overlap() {
        let mut dec = Decimator::new(8.0);
        dec.accumulate(&sample(0, 0.02, 0.03)); // sensor noise, not overlap
        dec.advance_to(1);
        assert_eq!(dec.frame.column(0).unwrap().overlap, 0.0);
        assert_eq!(dec.frame.overlap_columns, 0);
    }

    #[test]
    fn the_window_sample_count_falls_to_zero_when_the_sim_goes_quiet() {
        let mut dec = Decimator::new(8.0);
        for _ in 0..10 {
            dec.accumulate(&sample(0, 1.0, 0.0));
            dec.advance_to(dec.cur_bin + 1);
        }
        assert_eq!(dec.frame.samples, 10);
        // A full window with nothing arriving: the held trace is still drawn,
        // but the count must say it is a hold and not a measurement.
        dec.advance_to(dec.cur_bin + STRIP_COLUMNS as u64);
        assert_eq!(
            dec.frame.samples, 0,
            "stale samples were still counted as being in the window"
        );
    }

    #[test]
    fn a_long_stall_cannot_make_the_decimator_spin() {
        let mut dec = Decimator::new(8.0);
        // Ten million bins is ~87 hours. It must clamp, not iterate.
        let t = Instant::now();
        dec.advance_to(10_000_000);
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "catch-up was not clamped: {:?}",
            t.elapsed()
        );
        assert_eq!(dec.cur_bin, 10_000_000, "the clock did not resynchronise");
    }

    #[test]
    fn column_indices_out_of_range_return_none() {
        let f = StripFrame::default();
        assert!(f.is_empty());
        assert_eq!(f.column(0), None);
    }

    // ---- the thread -------------------------------------------------------

    #[test]
    fn the_logger_records_only_between_start_and_end() {
        let ring = Arc::new(SpscRing::<TelemetrySample>::new(4096));
        let mut h = spawn(
            Arc::clone(&ring),
            SessionConfig { max_samples: 100_000, window_s: 8.0, drain_batch: 4096 },
        )
        .expect("spawn");

        // Before start: drained and charted, but not retained.
        for i in 0..200 {
            ring.push(sample(i, 0.5, 0.0));
        }
        wait_until(|| ring.is_empty());
        assert_eq!(h.state(), SessionState::Idle);
        assert_eq!(h.logged(), 0, "samples were retained before start()");

        h.start();
        wait_until(|| h.state() == SessionState::Recording);
        for i in 0..500 {
            ring.push(sample(1000 + i, 1.0, 0.0));
        }
        wait_until(|| h.logged() >= 500);

        h.end();
        wait_until(|| h.state() == SessionState::Complete);
        let log = wait_for(|| h.take_log()).expect("a log should be handed over");
        assert_eq!(log.len(), 500);
        assert_eq!(log.iter().next().unwrap().t_capture_ns, 1000);

        // The chart keeps working after the session ends.
        for i in 0..50 {
            ring.push(sample(9000 + i, 0.25, 0.0));
        }
        wait_until(|| ring.is_empty());
        let frame = wait_for(|| {
            let f = h.strip();
            (f.window_s > 0.0).then_some(f)
        })
        .expect("the chart stopped publishing after the session ended");
        assert_eq!(frame.window_s, 8.0);
    }

    #[test]
    fn discard_leaves_nothing_to_collect() {
        let ring = Arc::new(SpscRing::<TelemetrySample>::new(1024));
        let h = spawn(Arc::clone(&ring), SessionConfig::default()).expect("spawn");
        h.start();
        wait_until(|| h.state() == SessionState::Recording);
        for i in 0..100 {
            ring.push(sample(i, 1.0, 0.0));
        }
        wait_until(|| h.logged() >= 100);
        h.discard();
        wait_until(|| h.state() == SessionState::Idle);
        assert!(h.take_log().is_none());
    }

    #[test]
    fn the_live_chart_fills_without_any_session_being_armed() {
        let ring = Arc::new(SpscRing::<TelemetrySample>::new(1024));
        let mut h = spawn(
            Arc::clone(&ring),
            // A 0.5 s window means columns close every ~2 ms, so the test does
            // not have to wait 8 seconds to see the chart populate.
            SessionConfig { max_samples: 4096, window_s: 0.5, drain_batch: 1024 },
        )
        .expect("spawn");
        for i in 0..64 {
            ring.push(sample(i, 0.9, 0.0));
        }
        let ok = wait_for(|| {
            let f = h.strip();
            (f.len() > 4 && f.columns().any(|c| c.thr_max > 0.8)).then_some(())
        });
        assert!(ok.is_some(), "the chart never picked up the pedal input");
        assert_eq!(h.state(), SessionState::Idle);
    }

    fn wait_until(mut p: impl FnMut() -> bool) {
        let t = Instant::now();
        while !p() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_for<T>(mut p: impl FnMut() -> Option<T>) -> Option<T> {
        let t = Instant::now();
        loop {
            if let Some(v) = p() {
                return Some(v);
            }
            if t.elapsed() > Duration::from_secs(5) {
                return None;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
