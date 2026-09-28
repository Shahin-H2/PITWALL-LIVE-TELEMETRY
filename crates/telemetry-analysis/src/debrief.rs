//! End-of-session debrief: lap segmentation, distance-aligned traces, and the
//! session KPIs.
//!
//! This is what a trackside engineer actually looks at after a run, and the
//! organising principle is borrowed from that workflow rather than from what is
//! convenient to compute:
//!
//! * **Traces are aligned by distance, not by time.** Two laps plotted against
//!   time diverge immediately and cannot be compared; plotted against distance
//!   round the lap, the same corner is at the same x on every lap. Distance is
//!   the only x-axis that makes a telemetry overlay mean anything.
//!
//! * **Pedal channels are decimated by peak, motion channels by mean.** Speed
//!   and longitudinal g are smooth, so averaging a bin is faithful. Throttle and
//!   brake are effectively switched signals where a 30 ms stab is the whole
//!   story, and averaging one into a 13 m bin would erase it.
//!
//! * **Nothing here is inferred.** Every figure is arithmetic over recorded
//!   samples. Where a simulator does not publish a channel the result is
//!   `None`, and the UI says so rather than showing a plausible zero.
//!
//! All of it runs on a worker thread, off the back of a finished
//! `telemetry_core::session::SessionLog`. The render thread never calls in here.

use telemetry_core::sample::{FieldMask, TelemetrySample};

/// Distance bins per trace. At 384 bins a 5 km lap resolves to ~13 m, which is
/// finer than one pixel per bin on any plot this is drawn into.
pub const TRACE_BINS: usize = 384;

/// Assumed sample rate when a capture carries no usable timestamps.
const FALLBACK_HZ: f32 = 60.0;

// ============================================================================
// Laps
// ============================================================================

#[derive(Clone, Debug)]
pub struct LapSummary {
    /// Lap number as the simulator reported it, or a synthetic 1-based index.
    pub index: u32,
    /// Index range into the sample slice, `[first, last]` inclusive.
    pub range: (usize, usize),
    pub time_s: f32,
    pub distance_m: f32,
    pub top_speed_kmh: f32,
    pub min_speed_kmh: f32,
    /// Sector splits, where the lap is long enough to divide into three.
    pub sectors_s: [Option<f32>; 3],
    /// False for the first and last laps of a session, which are almost always
    /// partial and must not be compared against complete ones.
    pub complete: bool,
}

/// How the distance axis was obtained. Worth surfacing: a lap measured by
/// integrating speed is honest but not as exact as one the sim measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceSource {
    /// The simulator publishes distance along the racing line.
    Reported,
    /// Derived by integrating speed over time.
    Integrated,
}

impl DistanceSource {
    pub const fn label(self) -> &'static str {
        match self {
            DistanceSource::Reported => "SIM DISTANCE",
            DistanceSource::Integrated => "INTEGRATED",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceScope {
    /// Aligned over one lap — the comparable case.
    Lap(u32),
    /// No lap markers in the capture, so the traces span the whole session.
    WholeSession,
}

// ============================================================================
// Traces
// ============================================================================

/// Four channels resampled onto a common distance axis.
#[derive(Clone)]
pub struct TraceSet {
    pub scope: TraceScope,
    pub distance_source: DistanceSource,
    /// Length of the distance axis in metres.
    pub distance_m: f32,
    /// Mean within the bin.
    pub speed_kmh: [f32; TRACE_BINS],
    /// Peak within the bin — see the module note on decimation.
    pub throttle: [f32; TRACE_BINS],
    pub brake: [f32; TRACE_BINS],
    /// Mean within the bin. Negative is braking.
    pub long_g: [f32; TRACE_BINS],
    /// Bins that had at least one sample. A bin with none is a gap in the data,
    /// not a zero, and is left out of the plot rather than drawn at the floor.
    pub valid: [bool; TRACE_BINS],
    pub speed_max: f32,
    pub g_range: f32,
}

impl Default for TraceSet {
    fn default() -> Self {
        Self {
            scope: TraceScope::WholeSession,
            distance_source: DistanceSource::Integrated,
            distance_m: 0.0,
            speed_kmh: [0.0; TRACE_BINS],
            throttle: [0.0; TRACE_BINS],
            brake: [0.0; TRACE_BINS],
            long_g: [0.0; TRACE_BINS],
            valid: [false; TRACE_BINS],
            speed_max: 1.0,
            g_range: 1.0,
        }
    }
}

impl TraceSet {
    pub fn valid_bins(&self) -> usize {
        self.valid.iter().filter(|v| **v).count()
    }
}

// ============================================================================
// Transitions
// ============================================================================

/// A population of measured transition delays, in milliseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Delays {
    pub count: usize,
    pub mean_ms: f32,
    /// Shortest delay — the driver at their most decisive.
    pub best_ms: f32,
    pub worst_ms: f32,
}

impl Delays {
    fn from(samples: &[f32]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let sum: f32 = samples.iter().sum();
        Some(Self {
            count: samples.len(),
            mean_ms: sum / samples.len() as f32,
            best_ms: samples.iter().copied().fold(f32::INFINITY, f32::min),
            worst_ms: samples.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TransitionStats {
    /// Coast time between lifting the throttle and touching the brake.
    pub throttle_to_brake: Option<Delays>,
    /// Coast time between releasing the brake and picking up the throttle.
    pub brake_to_throttle: Option<Delays>,
    /// Occasions both pedals were applied together.
    pub overlap_events: usize,
    pub overlap_total_s: f32,
    pub overlap_peak_s: f32,
}

/// A pedal is "off" below this and "on" above it. Two thresholds would add
/// hysteresis; at 60–300 Hz the extra machinery buys nothing, because a real
/// pedal crosses 4% once per press rather than chattering across it.
const PEDAL_ON: f32 = 0.04;
/// Reapplying the throttle above this cancels a pending lift — the driver
/// changed their mind and this was never a transition into braking.
const PEDAL_BACK_ON: f32 = 0.20;
/// A gap longer than this is not a transition, it is a straight.
const MAX_TRANSITION_S: f32 = 3.0;

// ============================================================================
// KPIs
// ============================================================================

#[derive(Clone, Debug, Default)]
pub struct SessionKpi {
    pub samples: usize,
    pub duration_s: f32,
    pub distance_m: f32,
    pub distance_source_reported: bool,
    /// Effective sample rate of the capture.
    pub rate_hz: f32,

    pub top_speed_kmh: f32,
    pub mean_speed_kmh: f32,
    /// Peak deceleration in g, measured under braking only.
    pub max_brake_g: Option<f32>,
    pub max_lat_g: Option<f32>,
    pub max_combined_g: Option<f32>,
    pub max_rpm: f32,

    pub laps_complete: usize,
    pub best_lap_s: Option<f32>,
    pub best_sectors_s: [Option<f32>; 3],
    /// The lap the best sectors came from, which need not be the best lap —
    /// the theoretical best is the sum of the fastest of each.
    pub theoretical_best_s: Option<f32>,

    /// Share of the session at full throttle, on the brakes, and doing neither.
    pub full_throttle_pct: f32,
    pub braking_pct: f32,
    pub coasting_pct: f32,
}

// ============================================================================
// The debrief
// ============================================================================

#[derive(Clone)]
pub struct Debrief {
    pub kpi: SessionKpi,
    pub traces: Box<TraceSet>,
    pub laps: Vec<LapSummary>,
    pub transitions: TransitionStats,
    /// Present when there was not enough data to say anything useful.
    pub warning: Option<String>,
}

/// Fewer samples than this and every figure is noise.
const MIN_SAMPLES: usize = 64;

impl Debrief {
    /// Analyse a finished session.
    ///
    /// Linear in the number of samples, with one `TRACE_BINS`-sized pass at the
    /// end. A 30-minute session at 300 Hz is a few tens of milliseconds, which
    /// is why this runs on a worker thread and reports when it is done rather
    /// than being called inline from a frame.
    pub fn compute(samples: &[TelemetrySample]) -> Debrief {
        if samples.len() < MIN_SAMPLES {
            return Debrief {
                kpi: SessionKpi { samples: samples.len(), ..Default::default() },
                traces: Box::default(),
                laps: Vec::new(),
                transitions: TransitionStats::default(),
                // Plain ASCII: these strings are drawn by the cluster, whose
                // glyph atlas has no em dash.
                warning: Some(format!(
                    "ONLY {} SAMPLES RECORDED - DRIVE FOR A FEW SECONDS WITH THE SIM CONNECTED",
                    samples.len()
                )),
            };
        }

        let dt = timebase(samples);
        let (distance, distance_source) = distance_axis(samples, &dt);
        let laps = segment_laps(samples, &dt, &distance);
        let transitions = measure_transitions(samples, &dt);
        let traces = build_traces(samples, &distance, &laps, distance_source);
        let kpi = summarise(samples, &dt, &distance, &laps, distance_source);

        let warning = if traces.valid_bins() * 4 < TRACE_BINS {
            Some("SPARSE DATA - THE TRACES COVER ONLY PART OF THE DISTANCE AXIS".into())
        } else {
            None
        };

        Debrief { kpi, traces: Box::new(traces), laps, transitions, warning }
    }
}

// ----------------------------------------------------------------------------
// Time
// ----------------------------------------------------------------------------

/// Per-sample durations in seconds, `dt[i]` being the interval sample `i`
/// represents.
///
/// Derived from the capture timestamps, which is the only clock that survives a
/// recording. When a capture has no usable timestamps at all — a synthetic
/// fixture, or a decoder that never stamped — this falls back to a nominal rate
/// rather than dividing by zero and producing infinities downstream.
fn timebase(s: &[TelemetrySample]) -> Vec<f32> {
    let span = s
        .last()
        .map(|l| l.t_capture_ns)
        .unwrap_or(0)
        .saturating_sub(s[0].t_capture_ns);
    if span == 0 {
        return vec![1.0 / FALLBACK_HZ; s.len()];
    }
    let mut out = Vec::with_capacity(s.len());
    for i in 0..s.len() {
        let d = if i + 1 < s.len() {
            s[i + 1].t_capture_ns.saturating_sub(s[i].t_capture_ns)
        } else if i > 0 {
            s[i].t_capture_ns.saturating_sub(s[i - 1].t_capture_ns)
        } else {
            0
        };
        // Clamp: a paused sim or a capture stitched from two runs can leave a
        // multi-second gap, and crediting one sample with ten seconds of
        // "braking" would poison every percentage in the KPI card.
        out.push((d as f32 / 1e9).clamp(0.0, 0.25));
    }
    out
}

// ----------------------------------------------------------------------------
// Distance
// ----------------------------------------------------------------------------

/// Cumulative distance in metres, one entry per sample.
fn distance_axis(s: &[TelemetrySample], dt: &[f32]) -> (Vec<f32>, DistanceSource) {
    let reported = s.iter().any(|x| x.has(FieldMask::LAP_DISTANCE))
        && s.iter().any(|x| x.lap_distance > 1.0);

    let mut out = Vec::with_capacity(s.len());
    if reported {
        // The sim's own distance resets each lap, so accumulate across resets.
        let mut base = 0.0f32;
        let mut prev = s[0].lap_distance;
        for x in s {
            let d = x.lap_distance;
            if d + 1.0 < prev {
                // Wrapped: the lap rolled over.
                base += prev;
            }
            prev = d;
            out.push(base + d);
        }
        (out, DistanceSource::Reported)
    } else {
        let mut acc = 0.0f32;
        for (x, d) in s.iter().zip(dt) {
            acc += x.speed_ms.max(0.0) * d;
            out.push(acc);
        }
        (out, DistanceSource::Integrated)
    }
}

// ----------------------------------------------------------------------------
// Laps
// ----------------------------------------------------------------------------

fn segment_laps(s: &[TelemetrySample], dt: &[f32], dist: &[f32]) -> Vec<LapSummary> {
    let mut bounds: Vec<usize> = vec![0];

    if s.iter().any(|x| x.has(FieldMask::LAP_NUMBER)) {
        for i in 1..s.len() {
            if s[i].lap_number != s[i - 1].lap_number {
                bounds.push(i);
            }
        }
    } else if s.iter().any(|x| x.has(FieldMask::LAP_DISTANCE)) {
        // No lap counter, but a distance that resets: a large backwards jump is
        // the start/finish line.
        let mut peak = 0.0f32;
        for i in 1..s.len() {
            peak = peak.max(s[i - 1].lap_distance);
            if peak > 100.0 && s[i].lap_distance < peak * 0.5 {
                bounds.push(i);
                peak = 0.0;
            }
        }
    }
    bounds.push(s.len());
    bounds.dedup();

    let n_laps = bounds.len() - 1;
    let mut laps = Vec::with_capacity(n_laps);
    for k in 0..n_laps {
        let a = bounds[k];
        let b = bounds[k + 1] - 1;
        if b <= a {
            continue;
        }
        let time_s: f32 = dt[a..=b].iter().sum();
        let distance_m = dist[b] - dist[a];
        let mut top = 0.0f32;
        let mut low = f32::INFINITY;
        for x in &s[a..=b] {
            top = top.max(x.speed_kmh());
            low = low.min(x.speed_kmh());
        }
        // A lap is only comparable if it has both of its boundaries. The first
        // and last of any session do not, because recording started and stopped
        // somewhere other than the line.
        let complete = n_laps > 2 && k > 0 && k + 1 < n_laps;
        let sectors_s = if complete {
            sector_splits(dt, dist, a, b)
        } else {
            [None, None, None]
        };
        laps.push(LapSummary {
            index: if s[a].has(FieldMask::LAP_NUMBER) {
                s[a].lap_number as u32
            } else {
                k as u32 + 1
            },
            range: (a, b),
            time_s,
            distance_m,
            top_speed_kmh: top,
            min_speed_kmh: if low.is_finite() { low } else { 0.0 },
            sectors_s,
            complete,
        });
    }
    laps
}

/// Split a lap into three equal distances and time each.
///
/// Equal thirds, not the circuit's real sector markers — no UDP protocol here
/// publishes those consistently, and three equal distances at least compare
/// like-for-like between laps of the same session, which is what the split is
/// for.
fn sector_splits(dt: &[f32], dist: &[f32], a: usize, b: usize) -> [Option<f32>; 3] {
    let total = dist[b] - dist[a];
    if total < 300.0 {
        return [None, None, None];
    }
    let mut out = [None, None, None];
    let mut edges = [a, b, b];
    for (k, frac) in [1.0f32 / 3.0, 2.0 / 3.0].iter().enumerate() {
        let target = dist[a] + total * frac;
        let mut idx = b;
        for i in a..=b {
            if dist[i] >= target {
                idx = i;
                break;
            }
        }
        edges[k] = idx;
    }
    let s1 = dt[a..edges[0]].iter().sum::<f32>();
    let s2 = dt[edges[0]..edges[1]].iter().sum::<f32>();
    let s3 = dt[edges[1]..=b].iter().sum::<f32>();
    if s1 > 0.0 && s2 > 0.0 && s3 > 0.0 {
        out = [Some(s1), Some(s2), Some(s3)];
    }
    out
}

// ----------------------------------------------------------------------------
// Transitions
// ----------------------------------------------------------------------------

fn measure_transitions(s: &[TelemetrySample], dt: &[f32]) -> TransitionStats {
    let mut t2b: Vec<f32> = Vec::new();
    let mut b2t: Vec<f32> = Vec::new();

    // Pending "throttle lifted" / "brake released" clocks, in seconds of coast.
    let mut lift: Option<f32> = None;
    let mut release: Option<f32> = None;
    let mut overlap_run = 0.0f32;
    let mut stats = TransitionStats::default();

    let mut prev_thr = s[0].throttle;
    let mut prev_brk = s[0].brake;

    for i in 1..s.len() {
        let thr = s[i].throttle;
        let brk = s[i].brake;
        let d = dt[i];

        // ---- overlap -------------------------------------------------------
        if thr > PEDAL_ON && brk > PEDAL_ON {
            overlap_run += d;
        } else if overlap_run > 0.0 {
            stats.overlap_events += 1;
            stats.overlap_total_s += overlap_run;
            stats.overlap_peak_s = stats.overlap_peak_s.max(overlap_run);
            overlap_run = 0.0;
        }

        // ---- throttle -> brake --------------------------------------------
        if prev_thr > PEDAL_ON && thr <= PEDAL_ON {
            lift = Some(0.0);
        }
        if let Some(acc) = lift.as_mut() {
            *acc += d;
            if prev_brk <= PEDAL_ON && brk > PEDAL_ON {
                t2b.push(*acc * 1000.0);
                lift = None;
            } else if thr > PEDAL_BACK_ON || *acc > MAX_TRANSITION_S {
                lift = None;
            }
        }

        // ---- brake -> throttle --------------------------------------------
        if prev_brk > PEDAL_ON && brk <= PEDAL_ON {
            release = Some(0.0);
        }
        if let Some(acc) = release.as_mut() {
            *acc += d;
            if prev_thr <= PEDAL_ON && thr > PEDAL_ON {
                b2t.push(*acc * 1000.0);
                release = None;
            } else if brk > PEDAL_BACK_ON || *acc > MAX_TRANSITION_S {
                release = None;
            }
        }

        prev_thr = thr;
        prev_brk = brk;
    }
    if overlap_run > 0.0 {
        stats.overlap_events += 1;
        stats.overlap_total_s += overlap_run;
        stats.overlap_peak_s = stats.overlap_peak_s.max(overlap_run);
    }

    stats.throttle_to_brake = Delays::from(&t2b);
    stats.brake_to_throttle = Delays::from(&b2t);
    stats
}

// ----------------------------------------------------------------------------
// Traces
// ----------------------------------------------------------------------------

fn build_traces(
    s: &[TelemetrySample],
    dist: &[f32],
    laps: &[LapSummary],
    source: DistanceSource,
) -> TraceSet {
    // Prefer the fastest complete lap: it is the lap the driver wants to look
    // at, and confining the traces to one lap is what makes the distance axis
    // comparable rather than a smear of the whole session.
    let best = laps
        .iter()
        .filter(|l| l.complete && l.time_s > 0.0)
        .min_by(|a, b| a.time_s.total_cmp(&b.time_s));

    let (a, b, scope) = match best {
        Some(l) => (l.range.0, l.range.1, TraceScope::Lap(l.index)),
        None => (0, s.len() - 1, TraceScope::WholeSession),
    };

    let d0 = dist[a];
    let span = (dist[b] - d0).max(1.0);

    let mut t = TraceSet {
        scope,
        distance_source: source,
        distance_m: span,
        ..Default::default()
    };

    let mut sum_speed = [0.0f32; TRACE_BINS];
    let mut sum_g = [0.0f32; TRACE_BINS];
    let mut count = [0u32; TRACE_BINS];
    let has_accel = s[a..=b].iter().any(|x| x.has(FieldMask::ACCEL));

    for i in a..=b {
        let f = ((dist[i] - d0) / span).clamp(0.0, 0.999_999);
        let k = (f * TRACE_BINS as f32) as usize;
        let k = k.min(TRACE_BINS - 1);
        sum_speed[k] += s[i].speed_kmh();
        if has_accel {
            sum_g[k] += s[i].long_g();
        }
        t.throttle[k] = t.throttle[k].max(s[i].throttle.clamp(0.0, 1.0));
        t.brake[k] = t.brake[k].max(s[i].brake.clamp(0.0, 1.0));
        count[k] += 1;
        t.valid[k] = true;
    }

    let mut speed_max = 1.0f32;
    let mut g_range = 0.25f32;
    for k in 0..TRACE_BINS {
        if count[k] == 0 {
            continue;
        }
        let n = count[k] as f32;
        t.speed_kmh[k] = sum_speed[k] / n;
        t.long_g[k] = sum_g[k] / n;
        speed_max = speed_max.max(t.speed_kmh[k]);
        g_range = g_range.max(t.long_g[k].abs());
    }
    t.speed_max = speed_max;
    t.g_range = g_range;
    t
}

// ----------------------------------------------------------------------------
// KPIs
// ----------------------------------------------------------------------------

fn summarise(
    s: &[TelemetrySample],
    dt: &[f32],
    dist: &[f32],
    laps: &[LapSummary],
    source: DistanceSource,
) -> SessionKpi {
    let duration_s: f32 = dt.iter().sum();
    let mut k = SessionKpi {
        samples: s.len(),
        duration_s,
        distance_m: dist[dist.len() - 1] - dist[0],
        distance_source_reported: source == DistanceSource::Reported,
        rate_hz: if duration_s > 0.0 { s.len() as f32 / duration_s } else { 0.0 },
        ..Default::default()
    };

    let mut speed_time = 0.0f32;
    let mut max_brake_g = 0.0f32;
    let mut saw_brake_g = false;
    let mut max_lat = 0.0f32;
    let mut max_comb = 0.0f32;
    let mut saw_accel = false;
    let mut t_full = 0.0f32;
    let mut t_brake = 0.0f32;
    let mut t_coast = 0.0f32;

    for (x, d) in s.iter().zip(dt) {
        k.top_speed_kmh = k.top_speed_kmh.max(x.speed_kmh());
        k.max_rpm = k.max_rpm.max(x.rpm);
        speed_time += x.speed_kmh() * d;

        if x.has(FieldMask::ACCEL) {
            saw_accel = true;
            max_lat = max_lat.max(x.lat_g().abs());
            max_comb = max_comb.max(x.combined_g());
            // Peak deceleration, qualified: on the brakes and actually moving.
            // Unqualified, this figure reports the impact at the end of the
            // session rather than the driver's best stop.
            if x.brake > PEDAL_ON && x.speed_ms > 5.0 {
                let decel = -x.long_g();
                if decel > 0.0 {
                    saw_brake_g = true;
                    max_brake_g = max_brake_g.max(decel);
                }
            }
        }

        if x.throttle > 0.97 {
            t_full += d;
        }
        if x.brake > PEDAL_ON {
            t_brake += d;
        }
        if x.throttle <= PEDAL_ON && x.brake <= PEDAL_ON {
            t_coast += d;
        }
    }

    if duration_s > 0.0 {
        k.mean_speed_kmh = speed_time / duration_s;
        k.full_throttle_pct = t_full / duration_s * 100.0;
        k.braking_pct = t_brake / duration_s * 100.0;
        k.coasting_pct = t_coast / duration_s * 100.0;
    }
    if saw_brake_g {
        k.max_brake_g = Some(max_brake_g);
    }
    if saw_accel {
        k.max_lat_g = Some(max_lat);
        k.max_combined_g = Some(max_comb);
    }

    let complete: Vec<&LapSummary> = laps.iter().filter(|l| l.complete).collect();
    k.laps_complete = complete.len();
    k.best_lap_s = complete
        .iter()
        .map(|l| l.time_s)
        .fold(None, |acc: Option<f32>, v| Some(acc.map_or(v, |a| a.min(v))));

    for si in 0..3 {
        k.best_sectors_s[si] = complete
            .iter()
            .filter_map(|l| l.sectors_s[si])
            .fold(None, |acc: Option<f32>, v| Some(acc.map_or(v, |a| a.min(v))));
    }
    if let [Some(a), Some(b), Some(c)] = k.best_sectors_s {
        k.theoretical_best_s = Some(a + b + c);
    }
    k
}

/// Format seconds as `m:ss.mmm`, the way a lap time is read.
pub fn lap_time(s: f32) -> String {
    if !s.is_finite() || s < 0.0 {
        return "--:--.---".into();
    }
    let m = (s / 60.0).floor();
    let rem = s - m * 60.0;
    format!("{m:.0}:{rem:06.3}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_core::sample::Vec3;

    /// Three laps of a synthetic circuit: one straight, one braking zone, one
    /// corner, repeated. Lap 2 is deliberately the quickest.
    fn synthetic(laps: usize, hz: f32) -> Vec<TelemetrySample> {
        let mut out = Vec::new();
        let mut t = 0.0f64;
        let per_lap = (hz * 30.0) as usize;
        for lap in 0..laps {
            // Lap 1 (index 1) runs 4% faster than the others.
            let pace = if lap == 1 { 1.04 } else { 1.0 };
            let mut lap_d = 0.0f32;
            for i in 0..per_lap {
                let ph = i as f32 / per_lap as f32;
                let (thr, brk, speed) = if ph < 0.35 {
                    (1.0, 0.0, 75.0 * pace)
                } else if ph < 0.42 {
                    (0.0, 0.92, 55.0 * pace)
                } else if ph < 0.62 {
                    (0.25, 0.0, 34.0 * pace)
                } else {
                    (1.0, 0.0, 62.0 * pace)
                };
                lap_d += speed / hz;
                out.push(TelemetrySample {
                    t_capture_ns: (t * 1e9) as u64,
                    is_active: true,
                    present: FieldMask::SPEED
                        | FieldMask::THROTTLE
                        | FieldMask::BRAKE
                        | FieldMask::ACCEL
                        | FieldMask::RPM
                        | FieldMask::LAP_NUMBER
                        | FieldMask::LAP_DISTANCE,
                    speed_ms: speed,
                    throttle: thr,
                    brake: brk,
                    // Longitudinal: decelerating hard under braking.
                    accel: Vec3::new(
                        if (0.42..0.62).contains(&ph) { 14.0 } else { 0.0 },
                        0.0,
                        if brk > 0.5 { -15.7 } else { 2.0 },
                    ),
                    rpm: 5000.0 + thr * 2500.0,
                    max_rpm: 8000.0,
                    lap_number: lap as u16 + 1,
                    lap_distance: lap_d,
                    ..Default::default()
                });
                t += 1.0 / hz as f64;
            }
        }
        out
    }

    #[test]
    fn a_short_capture_is_refused_rather_than_guessed_at() {
        let d = Debrief::compute(&synthetic(1, 60.0)[..10]);
        assert!(d.warning.is_some());
        assert_eq!(d.laps.len(), 0);
        assert!(d.kpi.best_lap_s.is_none());
    }

    #[test]
    fn laps_are_segmented_on_the_sims_own_counter() {
        let s = synthetic(4, 60.0);
        let d = Debrief::compute(&s);
        assert_eq!(d.laps.len(), 4, "got {:?}", d.laps.iter().map(|l| l.index).collect::<Vec<_>>());
        // First and last are partial by definition and must not be comparable.
        assert!(!d.laps[0].complete);
        assert!(!d.laps[3].complete);
        assert!(d.laps[1].complete && d.laps[2].complete);
        assert_eq!(d.kpi.laps_complete, 2);
    }

    #[test]
    fn the_best_lap_is_the_quickest_complete_one() {
        let s = synthetic(4, 60.0);
        let d = Debrief::compute(&s);
        let best = d.kpi.best_lap_s.expect("a best lap");
        let lap2 = d.laps.iter().find(|l| l.index == 2).unwrap();
        assert!((best - lap2.time_s).abs() < 1e-3, "best {best} vs lap2 {}", lap2.time_s);
        assert_eq!(d.traces.scope, TraceScope::Lap(2), "traces should align on the best lap");
    }

    #[test]
    fn sectors_sum_to_the_lap_time() {
        let s = synthetic(4, 60.0);
        let d = Debrief::compute(&s);
        let lap = d.laps.iter().find(|l| l.complete).unwrap();
        let [a, b, c] = lap.sectors_s;
        let sum = a.unwrap() + b.unwrap() + c.unwrap();
        assert!(
            (sum - lap.time_s).abs() < 1e-3,
            "sectors {sum} != lap {}",
            lap.time_s
        );
    }

    #[test]
    fn a_theoretical_best_is_never_slower_than_the_real_best() {
        let d = Debrief::compute(&synthetic(5, 60.0));
        let (Some(best), Some(theo)) = (d.kpi.best_lap_s, d.kpi.theoretical_best_s) else {
            panic!("expected both");
        };
        assert!(theo <= best + 1e-3, "theoretical {theo} slower than actual {best}");
    }

    #[test]
    fn distance_uses_the_sims_channel_when_it_has_one() {
        let d = Debrief::compute(&synthetic(3, 60.0));
        assert!(d.kpi.distance_source_reported);
        assert_eq!(d.traces.distance_source, DistanceSource::Reported);
        // Three 30 s laps averaging ~60 m/s is on the order of 5 km.
        assert!(d.kpi.distance_m > 3000.0, "got {} m", d.kpi.distance_m);
    }

    #[test]
    fn distance_is_integrated_when_the_sim_has_no_lap_channel() {
        let mut s = synthetic(3, 60.0);
        for x in &mut s {
            x.present = FieldMask::SPEED | FieldMask::THROTTLE | FieldMask::BRAKE;
            x.lap_distance = 0.0;
            x.lap_number = 0;
        }
        let d = Debrief::compute(&s);
        assert!(!d.kpi.distance_source_reported);
        assert_eq!(d.traces.scope, TraceScope::WholeSession, "no markers, so no lap alignment");
        // 90 s at roughly 60 m/s.
        assert!(
            (d.kpi.distance_m - 5000.0).abs() < 1500.0,
            "integrated distance looks wrong: {} m",
            d.kpi.distance_m
        );
    }

    #[test]
    fn traces_cover_the_distance_axis_and_stay_in_range() {
        let d = Debrief::compute(&synthetic(4, 60.0));
        let t = &d.traces;
        assert!(t.valid_bins() > TRACE_BINS * 3 / 4, "only {} bins filled", t.valid_bins());
        for k in 0..TRACE_BINS {
            assert!((0.0..=1.0).contains(&t.throttle[k]), "throttle out of range at {k}");
            assert!((0.0..=1.0).contains(&t.brake[k]), "brake out of range at {k}");
            assert!(t.speed_kmh[k].is_finite() && t.speed_kmh[k] >= 0.0);
            assert!(t.long_g[k].is_finite());
        }
        assert!(t.speed_max > 100.0);
    }

    #[test]
    fn pedal_traces_keep_the_peak_of_their_bin() {
        // A single full-brake sample inside the lap the traces align on. A mean
        // over its 13 m bin would bury it; the peak decimation must keep it.
        let mut s = synthetic(3, 60.0);
        for x in &mut s {
            x.brake = 0.0;
        }
        let d0 = Debrief::compute(&s);
        let TraceScope::Lap(_) = d0.traces.scope else { panic!("expected a lap scope") };
        let (a, b) = d0.laps.iter().find(|l| l.complete).unwrap().range;
        s[(a + b) / 2].brake = 1.0;
        let d = Debrief::compute(&s);
        assert!(
            d.traces.brake.iter().any(|b| *b > 0.9),
            "the single full-brake sample was averaged away"
        );
    }

    #[test]
    fn max_braking_force_is_measured_and_positive() {
        let d = Debrief::compute(&synthetic(3, 60.0));
        let g = d.kpi.max_brake_g.expect("braking g should be measured");
        assert!((g - 1.6).abs() < 0.1, "expected ~1.6 g, got {g}");
    }

    #[test]
    fn a_stationary_crash_spike_is_not_reported_as_braking() {
        let mut s = synthetic(3, 60.0);
        // A 9 g deceleration at walking pace: a wall, not a brake pedal.
        let n = s.len();
        s[n - 5].speed_ms = 1.0;
        s[n - 5].brake = 1.0;
        s[n - 5].accel = Vec3::new(0.0, 0.0, -88.0);
        let d = Debrief::compute(&s);
        let g = d.kpi.max_brake_g.unwrap();
        assert!(g < 3.0, "the impact leaked into max braking force: {g} g");
    }

    #[test]
    fn missing_accelerometer_data_reports_none_not_zero() {
        let mut s = synthetic(3, 60.0);
        for x in &mut s {
            x.present = FieldMask::SPEED | FieldMask::THROTTLE | FieldMask::BRAKE;
        }
        let d = Debrief::compute(&s);
        assert!(d.kpi.max_brake_g.is_none());
        assert!(d.kpi.max_lat_g.is_none());
        assert!(d.kpi.max_combined_g.is_none());
    }

    #[test]
    fn throttle_to_brake_delay_matches_the_constructed_coast() {
        // Lift at t=0, brake 120 ms later, repeated.
        let hz = 200.0f32;
        let mut s = Vec::new();
        for lap in 0..5 {
            for i in 0..200 {
                let t = (lap * 200 + i) as f32 / hz;
                // 0.00-0.50 s throttle, 0.50-0.62 s coast, 0.62-1.00 s brake.
                let ph = i as f32 / hz;
                let (thr, brk) = if ph < 0.5 {
                    (1.0, 0.0)
                } else if ph < 0.62 {
                    (0.0, 0.0)
                } else {
                    (0.0, 0.9)
                };
                s.push(TelemetrySample {
                    t_capture_ns: (t as f64 * 1e9) as u64,
                    present: FieldMask::THROTTLE | FieldMask::BRAKE | FieldMask::SPEED,
                    throttle: thr,
                    brake: brk,
                    speed_ms: 50.0,
                    ..Default::default()
                });
            }
        }
        let d = Debrief::compute(&s);
        let t2b = d.transitions.throttle_to_brake.expect("transitions found");
        assert!(t2b.count >= 4, "found only {} transitions", t2b.count);
        assert!(
            (t2b.mean_ms - 120.0).abs() < 15.0,
            "expected ~120 ms, got {:.1} ms",
            t2b.mean_ms
        );
        assert_eq!(d.transitions.overlap_events, 0, "no overlap was constructed");
    }

    #[test]
    fn a_lift_that_goes_back_to_throttle_is_not_a_transition() {
        let hz = 100.0f32;
        let mut s = Vec::new();
        for i in 0..600 {
            let t = i as f32 / hz;
            // Brief lifts, then straight back on the power. Never a brake.
            let thr = if (i / 20) % 2 == 0 { 1.0 } else { 0.0 };
            s.push(TelemetrySample {
                t_capture_ns: (t as f64 * 1e9) as u64,
                present: FieldMask::THROTTLE | FieldMask::BRAKE,
                throttle: thr,
                brake: 0.0,
                speed_ms: 50.0,
                ..Default::default()
            });
        }
        let d = Debrief::compute(&s);
        assert!(
            d.transitions.throttle_to_brake.is_none(),
            "lifts with no braking were counted as transitions"
        );
    }

    #[test]
    fn pedal_overlap_is_detected_and_timed() {
        let hz = 100.0f32;
        let mut s = Vec::new();
        for i in 0..400 {
            let t = i as f32 / hz;
            // Two 200 ms episodes of both pedals together.
            let overlapping = (100..120).contains(&i) || (300..320).contains(&i);
            s.push(TelemetrySample {
                t_capture_ns: (t as f64 * 1e9) as u64,
                present: FieldMask::THROTTLE | FieldMask::BRAKE,
                throttle: if overlapping { 0.5 } else { 1.0 },
                brake: if overlapping { 0.4 } else { 0.0 },
                speed_ms: 50.0,
                ..Default::default()
            });
        }
        let d = Debrief::compute(&s);
        assert_eq!(d.transitions.overlap_events, 2);
        assert!(
            (d.transitions.overlap_total_s - 0.4).abs() < 0.05,
            "got {} s",
            d.transitions.overlap_total_s
        );
    }

    #[test]
    fn percentages_are_a_partition_of_the_session() {
        let d = Debrief::compute(&synthetic(3, 60.0));
        let k = &d.kpi;
        assert!(k.full_throttle_pct > 30.0, "got {}", k.full_throttle_pct);
        assert!(k.braking_pct > 3.0 && k.braking_pct < 20.0, "got {}", k.braking_pct);
        // Coasting and braking are mutually exclusive and neither can exceed
        // the session.
        assert!(k.braking_pct + k.coasting_pct <= 100.5);
        assert!((k.rate_hz - 60.0).abs() < 1.0, "rate {}", k.rate_hz);
    }

    #[test]
    fn a_multi_second_gap_cannot_inflate_a_percentage() {
        let mut s = synthetic(3, 60.0);
        // Simulate the sim being paused for an hour mid-session.
        let n = s.len();
        for x in s[n / 2..].iter_mut() {
            x.t_capture_ns += 3_600_000_000_000;
        }
        let d = Debrief::compute(&s);
        assert!(
            d.kpi.duration_s < 200.0,
            "the pause was counted as driving: {} s",
            d.kpi.duration_s
        );
        assert!(d.kpi.braking_pct <= 100.0);
    }

    #[test]
    fn an_unstamped_capture_falls_back_to_a_nominal_rate() {
        let mut s = synthetic(3, 60.0);
        for x in &mut s {
            x.t_capture_ns = 0;
        }
        let d = Debrief::compute(&s);
        assert!(d.kpi.duration_s > 0.0, "duration collapsed to zero");
        assert!((d.kpi.rate_hz - FALLBACK_HZ).abs() < 1.0);
        for l in &d.laps {
            assert!(l.time_s.is_finite() && l.time_s > 0.0);
        }
    }

    #[test]
    fn lap_times_format_the_way_a_driver_reads_them() {
        assert_eq!(lap_time(92.456), "1:32.456");
        assert_eq!(lap_time(5.0), "0:05.000");
        assert_eq!(lap_time(f32::NAN), "--:--.---");
    }
}
