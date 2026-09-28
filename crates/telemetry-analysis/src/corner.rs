//! Corner segmentation and per-corner driver metrics.
//!
//! Everything in this module is deterministic arithmetic. No inference, no
//! network, no fitted parameters. That is the point: these numbers are
//! *measured*, and every recommendation the setup analyzer makes traces back to
//! one of them. A driver can be shown the corner that produced the advice.

use telemetry_core::sample::{FieldMask, TelemetrySample};

/// Below this lateral load the car is considered to be travelling straight.
const CORNER_ENTRY_G: f32 = 0.35;
/// Hysteresis: once in a corner we don't exit until load drops below this.
/// Without hysteresis, a corner with a brief mid-load dip splits into two.
const CORNER_EXIT_G: f32 = 0.25;
/// Ignore transients shorter than this — kerbs, bumps, a twitch of steering.
const MIN_CORNER_S: f32 = 0.35;
/// Pedal travel under which we treat the pedal as released.
const PEDAL_DEADZONE: f32 = 0.05;

/// Which part of the corner a sample belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Braking in a straight line, before meaningful lateral load.
    Braking,
    /// Turn-in through to the speed minimum. Where trail-braking lives.
    Entry,
    /// Around the speed minimum.
    Apex,
    /// Unwinding steering and applying power.
    Exit,
}

/// One measured corner.
#[derive(Clone, Debug)]
pub struct Corner {
    pub index: usize,
    pub t_start_s: f32,
    pub t_apex_s: f32,
    pub t_end_s: f32,

    pub entry_speed_ms: f32,
    pub min_speed_ms: f32,
    pub exit_speed_ms: f32,
    /// Signed: positive = a right-hand corner.
    pub peak_lat_g: f32,

    // ---- braking behaviour ----
    /// Peak brake pressure, 0..1.
    pub peak_brake: f32,
    /// Fastest rate of brake application, in units of pedal travel per second.
    /// High values mean a driver who "hits" the pedal; low means a squeeze.
    pub brake_onset_rate: f32,
    /// How long from first brake input to peak pressure.
    pub time_to_peak_brake_s: f32,
    /// ∫ brake · |lat g| dt over entry, normalised by entry duration.
    ///
    /// This is the trail-braking index. Near zero means the driver finishes
    /// braking in a straight line and turns in on a trailing throttle; high
    /// values mean they carry brake pressure deep into the corner to rotate
    /// the car. It is the single most characteristic number in a driver's
    /// fingerprint, and it dictates how much entry stability the setup needs.
    pub trail_brake_index: f32,

    // ---- the gap ----
    /// Seconds spent with neither pedal meaningfully applied.
    ///
    /// Coast time is dead time: the car is neither slowing under control nor
    /// accelerating. It correlates with lap time about as strongly as any
    /// single scalar you can extract, and unlike most things a driver coach
    /// says, it is unambiguous.
    pub coast_time_s: f32,

    // ---- power application ----
    /// Seconds from the speed minimum to first meaningful throttle.
    pub time_to_throttle_s: f32,
    /// Fastest rate of throttle application on exit, travel per second.
    pub throttle_ramp_rate: f32,
    /// Throttle at the moment of the speed minimum — the mark of a driver who
    /// is already back on power at the apex.
    pub throttle_at_apex: f32,

    // ---- balance ----
    /// Mean slip-angle balance during entry. Positive = oversteer.
    pub balance_entry: Option<f32>,
    pub balance_apex: Option<f32>,
    pub balance_exit: Option<f32>,

    /// Steering direction changes per second — a proxy for how settled the car
    /// felt. A driver constantly correcting is fighting the setup.
    pub steering_reversals_per_s: f32,
}

impl Corner {
    pub fn duration_s(&self) -> f32 {
        self.t_end_s - self.t_start_s
    }

    /// Convenience: is this corner's balance dominated by understeer?
    pub fn understeers_on_entry(&self, threshold_rad: f32) -> bool {
        self.balance_entry.map(|b| b < -threshold_rad).unwrap_or(false)
    }

    pub fn oversteers_on_exit(&self, threshold_rad: f32) -> bool {
        self.balance_exit.map(|b| b > threshold_rad).unwrap_or(false)
    }
}

/// Segment a contiguous run of samples into corners.
///
/// Expects samples in time order from a single lap. Straights simply produce
/// no corners.
pub fn segment(samples: &[TelemetrySample]) -> Vec<Corner> {
    if samples.len() < 8 {
        return Vec::new();
    }

    // A light smoothing pass on lateral load. Raw lat-g from any sim is noisy
    // enough that an unsmoothed threshold crossing fires several times per
    // corner; a 5-sample moving average is enough to stabilise it without
    // meaningfully delaying the edge.
    let lat: Vec<f32> = smooth(&samples.iter().map(|s| s.lat_g()).collect::<Vec<_>>(), 5);

    let mut corners = Vec::new();
    let mut i = 0usize;
    let mut index = 0usize;

    while i < samples.len() {
        if lat[i].abs() < CORNER_ENTRY_G {
            i += 1;
            continue;
        }
        // Walk forward to the end of sustained lateral load.
        let start = i;
        let mut end = i;
        while end + 1 < samples.len() && lat[end + 1].abs() > CORNER_EXIT_G {
            end += 1;
        }

        let t0 = t_of(&samples[start]);
        let t1 = t_of(&samples[end]);
        if t1 - t0 >= MIN_CORNER_S {
            // Extend backwards to catch the braking zone that precedes turn-in.
            let brake_start = walk_back_to_brake_release(samples, start);
            if let Some(c) = measure(samples, &lat, brake_start, end, index) {
                corners.push(c);
                index += 1;
            }
        }
        i = end + 1;
    }

    corners
}

#[inline]
fn t_of(s: &TelemetrySample) -> f32 {
    s.t_capture_ns as f32 / 1e9
}

/// Walk backwards from turn-in to the moment the driver first touched the
/// brake, so the braking zone is included in the corner.
fn walk_back_to_brake_release(samples: &[TelemetrySample], from: usize) -> usize {
    let mut i = from;
    let mut seen_brake = false;
    while i > 0 {
        let b = samples[i].brake;
        if b > PEDAL_DEADZONE {
            seen_brake = true;
        } else if seen_brake {
            // We've walked back past the start of the brake application.
            return i;
        }
        // Don't reach back more than ~4 s; that's a whole straight.
        if t_of(&samples[from]) - t_of(&samples[i]) > 4.0 {
            break;
        }
        i -= 1;
    }
    i
}

fn measure(
    samples: &[TelemetrySample],
    lat: &[f32],
    start: usize,
    end: usize,
    index: usize,
) -> Option<Corner> {
    let win = samples.get(start..=end)?;
    if win.len() < 4 {
        return None;
    }

    // Apex := the speed minimum.
    let apex_rel = win
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.speed_ms.total_cmp(&b.1.speed_ms))
        .map(|(i, _)| i)?;
    let apex = start + apex_rel;

    let t_start = t_of(&win[0]);
    let t_apex = t_of(&samples[apex]);
    let t_end = t_of(&win[win.len() - 1]);

    // ---- braking ----------------------------------------------------------
    let mut peak_brake = 0.0f32;
    let mut brake_onset_rate = 0.0f32;
    let mut first_brake_t: Option<f32> = None;
    let mut peak_brake_t = t_start;

    for w in win.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let dt = (t_of(b) - t_of(a)).max(1e-4);
        let rate = (b.brake - a.brake) / dt;
        if rate > brake_onset_rate {
            brake_onset_rate = rate;
        }
        if b.brake > PEDAL_DEADZONE && first_brake_t.is_none() {
            first_brake_t = Some(t_of(b));
        }
        if b.brake > peak_brake {
            peak_brake = b.brake;
            peak_brake_t = t_of(b);
        }
    }
    let time_to_peak_brake_s = first_brake_t.map(|t| peak_brake_t - t).unwrap_or(0.0).max(0.0);

    // ---- trail braking ----------------------------------------------------
    // Integrated over turn-in to apex only: brake pressure on a straight is
    // not trail braking, it's just braking.
    let mut trail_accum = 0.0f32;
    let mut trail_dur = 0.0f32;
    for k in start..apex {
        let dt = (t_of(&samples[k + 1]) - t_of(&samples[k])).max(0.0);
        if lat[k].abs() > CORNER_EXIT_G {
            trail_accum += samples[k].brake * lat[k].abs() * dt;
            trail_dur += dt;
        }
    }
    let trail_brake_index = if trail_dur > 1e-3 { trail_accum / trail_dur } else { 0.0 };

    // ---- coast ------------------------------------------------------------
    let mut coast_time_s = 0.0f32;
    for k in start..end {
        let s = &samples[k];
        let dt = (t_of(&samples[k + 1]) - t_of(s)).max(0.0);
        if s.throttle < PEDAL_DEADZONE && s.brake < PEDAL_DEADZONE {
            coast_time_s += dt;
        }
    }

    // ---- throttle application --------------------------------------------
    let mut time_to_throttle_s = t_end - t_apex;
    for k in apex..=end {
        if samples[k].throttle > PEDAL_DEADZONE {
            time_to_throttle_s = t_of(&samples[k]) - t_apex;
            break;
        }
    }
    let mut throttle_ramp_rate = 0.0f32;
    for k in apex..end {
        let dt = (t_of(&samples[k + 1]) - t_of(&samples[k])).max(1e-4);
        let rate = (samples[k + 1].throttle - samples[k].throttle) / dt;
        if rate > throttle_ramp_rate {
            throttle_ramp_rate = rate;
        }
    }

    // ---- balance by phase -------------------------------------------------
    let balance_entry = mean_balance(&samples[start..apex.max(start)]);
    let apex_lo = apex.saturating_sub(3).max(start);
    let apex_hi = (apex + 3).min(end);
    let balance_apex = mean_balance(&samples[apex_lo..=apex_hi]);
    let balance_exit = mean_balance(&samples[apex..=end]);

    // ---- steering activity ------------------------------------------------
    let mut reversals = 0u32;
    for w in win.windows(3) {
        let d1 = w[1].steer - w[0].steer;
        let d2 = w[2].steer - w[1].steer;
        if d1 * d2 < 0.0 && d1.abs() > 0.01 && d2.abs() > 0.01 {
            reversals += 1;
        }
    }
    let dur = (t_end - t_start).max(1e-3);

    let peak_lat_g = lat[start..=end]
        .iter()
        .copied()
        .max_by(|a, b| a.abs().total_cmp(&b.abs()))
        .unwrap_or(0.0);

    Some(Corner {
        index,
        t_start_s: t_start,
        t_apex_s: t_apex,
        t_end_s: t_end,
        entry_speed_ms: win[0].speed_ms,
        min_speed_ms: samples[apex].speed_ms,
        exit_speed_ms: win[win.len() - 1].speed_ms,
        peak_lat_g,
        peak_brake,
        brake_onset_rate,
        time_to_peak_brake_s,
        trail_brake_index,
        coast_time_s,
        time_to_throttle_s,
        throttle_ramp_rate,
        throttle_at_apex: samples[apex].throttle,
        balance_entry,
        balance_apex,
        balance_exit,
        steering_reversals_per_s: reversals as f32 / dur,
    })
}

/// Mean slip-angle balance over a window, **weighted by lateral load**.
///
/// The weighting is not a refinement, it is required for the number to mean
/// anything. Slip angle is generated by lateral load; during straight-line
/// braking both axles sit at ~0° and the balance is structurally zero. A plain
/// arithmetic mean therefore averages a real handling trait against a run of
/// meaningless zeros and halves it — enough, in practice, to push genuine
/// understeer below the significance threshold and have the app report a car
/// as neutral when the driver can plainly feel that it is not.
///
/// Weighting by |lat g| makes the figure what a race engineer means by
/// "balance on entry": the balance *while the car is loaded*, with the
/// straight-line portion contributing nothing because it has nothing to say.
fn mean_balance(win: &[TelemetrySample]) -> Option<f32> {
    let mut weighted_sum = 0.0f32;
    let mut weight_total = 0.0f32;
    for s in win {
        if !s.has(FieldMask::SLIP_ANGLE) {
            continue;
        }
        let Some(b) = s.balance() else { continue };
        // Below this the car is essentially straight and the sample carries
        // no balance information at all.
        let w = s.lat_g().abs();
        if w < 0.15 {
            continue;
        }
        weighted_sum += b * w;
        weight_total += w;
    }
    (weight_total > 1e-4).then(|| weighted_sum / weight_total)
}

/// Centred moving average. Window is clamped at the edges rather than padded,
/// so the first and last samples aren't dragged toward zero.
fn smooth(v: &[f32], window: usize) -> Vec<f32> {
    if window <= 1 || v.len() < window {
        return v.to_vec();
    }
    let half = window / 2;
    (0..v.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(v.len());
            let slice = &v[lo..hi];
            slice.iter().sum::<f32>() / slice.len() as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_core::sample::{SimId, Vec3};

    /// Build a synthetic lap: straight, brake, corner, power out, straight.
    fn synthetic_corner() -> Vec<TelemetrySample> {
        let mut out = Vec::new();
        let hz = 60.0f32;
        let mut t = 0.0f32;
        let push = |t: f32, speed: f32, thr: f32, brk: f32, lat: f32, steer: f32| {
            TelemetrySample {
                t_capture_ns: (t * 1e9) as u64,
                sim: SimId::Forza,
                is_active: true,
                present: FieldMask::SPEED
                    | FieldMask::THROTTLE
                    | FieldMask::BRAKE
                    | FieldMask::ACCEL
                    | FieldMask::STEER,
                speed_ms: speed,
                throttle: thr,
                brake: brk,
                steer,
                accel: Vec3::new(lat * 9.80665, 0.0, 0.0),
                ..Default::default()
            }
        };

        // 1s flat out on the straight
        for _ in 0..60 {
            out.push(push(t, 70.0, 1.0, 0.0, 0.0, 0.0));
            t += 1.0 / hz;
        }
        // 0.5s braking, straight
        for i in 0..30 {
            let f = i as f32 / 30.0;
            out.push(push(t, 70.0 - 20.0 * f, 0.0, 0.9, 0.0, 0.0));
            t += 1.0 / hz;
        }
        // 0.5s turn-in with trailing brake
        for i in 0..30 {
            let f = i as f32 / 30.0;
            out.push(push(t, 50.0 - 10.0 * f, 0.0, 0.6 * (1.0 - f), 1.0 + f, 0.3));
            t += 1.0 / hz;
        }
        // 0.3s coasting at apex — the dead time we want to detect
        for _ in 0..18 {
            out.push(push(t, 40.0, 0.0, 0.0, 1.8, 0.4));
            t += 1.0 / hz;
        }
        // 0.7s power down on exit
        for i in 0..42 {
            let f = i as f32 / 42.0;
            out.push(push(t, 40.0 + 25.0 * f, f, 0.0, 1.6 * (1.0 - f * 0.6), 0.3 * (1.0 - f)));
            t += 1.0 / hz;
        }
        // straight again
        for _ in 0..60 {
            out.push(push(t, 68.0, 1.0, 0.0, 0.0, 0.0));
            t += 1.0 / hz;
        }
        out
    }

    #[test]
    fn finds_exactly_one_corner() {
        let c = segment(&synthetic_corner());
        assert_eq!(c.len(), 1, "expected one corner, got {}", c.len());
    }

    #[test]
    fn measures_the_speed_minimum_at_the_apex() {
        let c = &segment(&synthetic_corner())[0];
        assert!(
            (c.min_speed_ms - 40.0).abs() < 1.0,
            "apex speed {} should be ~40 m/s",
            c.min_speed_ms
        );
        assert!(c.exit_speed_ms > c.min_speed_ms);
    }

    #[test]
    fn detects_the_coasting_gap() {
        let c = &segment(&synthetic_corner())[0];
        assert!(
            c.coast_time_s > 0.25 && c.coast_time_s < 0.45,
            "coast time {} should be ~0.3 s",
            c.coast_time_s
        );
    }

    #[test]
    fn trail_braking_is_positive_when_brake_overlaps_lateral_load() {
        let c = &segment(&synthetic_corner())[0];
        assert!(
            c.trail_brake_index > 0.0,
            "driver carries brake into the corner, index was {}",
            c.trail_brake_index
        );
    }

    #[test]
    fn balance_is_not_diluted_by_the_straight_line_braking_zone() {
        // Same car, two windows: one purely loaded, one with a long
        // zero-load braking run prepended. The measured balance must agree.
        let loaded: Vec<_> = (0..36)
            .map(|i| {
                let lat = 1.0 + i as f32 / 36.0 * 0.9;
                let base = lat * 0.030;
                TelemetrySample {
                    present: FieldMask::SLIP_ANGLE | FieldMask::ACCEL,
                    accel: Vec3::new(lat * 9.80665, 0.0, 0.0),
                    // front slides twice the rear => understeer
                    slip_angle: [base, base, base * 0.5, base * 0.5],
                    ..Default::default()
                }
            })
            .collect();
        let mut with_braking: Vec<TelemetrySample> = (0..30)
            .map(|_| TelemetrySample {
                present: FieldMask::SLIP_ANGLE | FieldMask::ACCEL,
                accel: Vec3::ZERO,
                slip_angle: [0.0; 4],
                ..Default::default()
            })
            .collect();
        with_braking.extend(loaded.iter().copied());

        let a = mean_balance(&loaded).unwrap();
        let b = mean_balance(&with_braking).unwrap();
        assert!(
            (a - b).abs() < 1e-4,
            "prepending a braking zone changed the balance from {a} to {b}"
        );
        assert!(a < -0.02, "should read as significant understeer, got {a}");
    }

    #[test]
    fn a_straight_produces_no_corners() {
        let flat: Vec<_> = (0..300)
            .map(|i| TelemetrySample {
                t_capture_ns: (i as u64) * 16_666_667,
                speed_ms: 80.0,
                throttle: 1.0,
                ..Default::default()
            })
            .collect();
        assert!(segment(&flat).is_empty());
    }

    #[test]
    fn empty_and_tiny_inputs_are_safe() {
        assert!(segment(&[]).is_empty());
        assert!(segment(&[TelemetrySample::default()]).is_empty());
    }
}
