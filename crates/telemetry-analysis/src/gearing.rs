//! Gear ratio analysis.
//!
//! Of every recommendation this app can make, gearing is the one that is
//! closest to objectively correct, because it barely involves the driver at
//! all. Whether the car hits the limiter 300 m before the braking zone is a
//! measurable fact, not a matter of feel, so it is computed exactly rather than
//! estimated.

use telemetry_core::sample::{FieldMask, TelemetrySample};

/// Fraction of redline above which we consider the engine to be on the limiter.
const LIMITER_FRACTION: f32 = 0.995;
/// If the highest gear never reaches this fraction of redline on the longest
/// straight, the ratio is too long and the car is leaving acceleration unused.
const UNDER_REVVED_FRACTION: f32 = 0.94;

#[derive(Clone, Debug, Default)]
pub struct GearStats {
    pub gear: i8,
    /// Seconds spent in this gear.
    pub time_s: f32,
    pub min_rpm: f32,
    pub max_rpm: f32,
    pub mean_rpm: f32,
    /// Seconds spent bouncing off the limiter in this gear.
    pub limiter_time_s: f32,
    /// Highest speed reached in this gear, m/s.
    pub top_speed_ms: f32,
}

#[derive(Clone, Debug, Default)]
pub struct GearingReport {
    pub per_gear: Vec<GearStats>,
    pub redline_rpm: f32,
    /// Total time on the limiter across the lap.
    pub total_limiter_time_s: f32,
    /// Highest gear the driver actually used.
    pub top_gear_used: i8,
    /// Peak RPM reached in that gear as a fraction of redline.
    pub top_gear_peak_fraction: f32,
    /// RPM histogram, 32 buckets from idle to redline. Feeds the UI plot, and
    /// is the compact picture of where the engine actually spends its time.
    pub rpm_histogram: [f32; 32],
    pub verdict: GearingVerdict,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum GearingVerdict {
    #[default]
    InsufficientData,
    /// Hitting the limiter with straight left — lengthen the final drive.
    TooShort {
        limiter_seconds_x100: u32,
    },
    /// Never reaching the power band in top — shorten the final drive.
    TooLong {
        peak_fraction_x100: u32,
    },
    /// Within a sensible window.
    WellMatched,
}

impl GearingReport {
    /// A one-line human summary. The structured fields above are what the
    /// report and the JSON export carry; the UI wants a sentence.
    pub fn headline(&self) -> String {
        match &self.verdict {
            GearingVerdict::InsufficientData => {
                "Not enough clean laps to judge gearing yet.".into()
            }
            GearingVerdict::TooShort { limiter_seconds_x100 } => format!(
                "Top gear is short — {:.2}s on the limiter. Lengthen the final drive.",
                *limiter_seconds_x100 as f32 / 100.0
            ),
            GearingVerdict::TooLong { peak_fraction_x100 } => format!(
                "Top gear is long — peaks at {}% of redline. Shorten the final drive.",
                peak_fraction_x100
            ),
            GearingVerdict::WellMatched => {
                "Gearing is well matched to this track.".into()
            }
        }
    }
}

pub fn analyse(samples: &[TelemetrySample]) -> GearingReport {
    let mut report = GearingReport::default();
    if samples.len() < 32 {
        return report;
    }

    // Establish the redline. Forza and AC publish it; F1 does not, so the
    // decoder tracks a running maximum which we fall back to.
    let redline = samples
        .iter()
        .map(|s| if s.max_rpm > 1.0 { s.max_rpm } else { s.rpm })
        .fold(0.0f32, f32::max);
    if redline < 1000.0 {
        return report;
    }
    report.redline_rpm = redline;

    // Accumulate per gear. Index 0..=9 maps to gears 1..=10; reverse and
    // neutral are excluded because their ratios are not what we're tuning.
    let mut acc: Vec<(f32, f32, f32, f32, f32, f32)> = vec![(0.0, f32::MAX, 0.0, 0.0, 0.0, 0.0); 11];
    let mut hist_weight = [0.0f32; 32];
    let mut total_limiter = 0.0f32;

    for w in samples.windows(2) {
        let (s, next) = (&w[0], &w[1]);
        if !s.has(FieldMask::GEAR) || !s.has(FieldMask::RPM) || !s.is_active {
            continue;
        }
        let dt = ((next.t_capture_ns.saturating_sub(s.t_capture_ns)) as f32 / 1e9)
            .clamp(0.0, 0.2); // guard against a pause in the capture
        if dt <= 0.0 {
            continue;
        }

        let bucket = ((s.rpm / redline) * 31.0).clamp(0.0, 31.0) as usize;
        hist_weight[bucket] += dt;

        let on_limiter = s.rpm >= redline * LIMITER_FRACTION;
        if on_limiter {
            total_limiter += dt;
        }

        let g = s.gear;
        if g < 1 || g as usize > 10 {
            continue;
        }
        let e = &mut acc[g as usize];
        e.0 += dt; // time
        e.1 = e.1.min(s.rpm); // min rpm
        e.2 = e.2.max(s.rpm); // max rpm
        e.3 += s.rpm * dt; // rpm-seconds, for the time-weighted mean
        e.4 += if on_limiter { dt } else { 0.0 };
        e.5 = e.5.max(s.speed_ms);
    }

    let total_time: f32 = hist_weight.iter().sum();
    if total_time > 0.0 {
        for (i, w) in hist_weight.iter().enumerate() {
            report.rpm_histogram[i] = w / total_time;
        }
    }
    report.total_limiter_time_s = total_limiter;

    for (gear, e) in acc.iter().enumerate().skip(1) {
        if e.0 <= 0.0 {
            continue;
        }
        report.per_gear.push(GearStats {
            gear: gear as i8,
            time_s: e.0,
            min_rpm: if e.1 == f32::MAX { 0.0 } else { e.1 },
            max_rpm: e.2,
            mean_rpm: e.3 / e.0,
            limiter_time_s: e.4,
            top_speed_ms: e.5,
        });
    }

    // The "top gear" for tuning purposes is the highest gear the driver spent
    // real time in — not one they brushed for a tenth over a crest.
    let Some(top) = report
        .per_gear
        .iter()
        .filter(|g| g.time_s > 0.5)
        .max_by_key(|g| g.gear)
    else {
        return report;
    };

    report.top_gear_used = top.gear;
    report.top_gear_peak_fraction = top.max_rpm / redline;

    report.verdict = if top.limiter_time_s > 0.30 {
        GearingVerdict::TooShort {
            limiter_seconds_x100: (top.limiter_time_s * 100.0) as u32,
        }
    } else if report.top_gear_peak_fraction < UNDER_REVVED_FRACTION {
        GearingVerdict::TooLong {
            peak_fraction_x100: (report.top_gear_peak_fraction * 100.0) as u32,
        }
    } else {
        GearingVerdict::WellMatched
    };

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_core::sample::SimId;

    fn lap(gear: i8, rpm_at_end: f32, redline: f32, seconds: f32) -> Vec<TelemetrySample> {
        let hz = 60.0f32;
        let n = (seconds * hz) as usize;
        (0..n)
            .map(|i| {
                let f = i as f32 / n as f32;
                TelemetrySample {
                    t_capture_ns: (i as f32 / hz * 1e9) as u64,
                    sim: SimId::Forza,
                    is_active: true,
                    present: FieldMask::GEAR | FieldMask::RPM | FieldMask::SPEED,
                    gear,
                    rpm: 3000.0 + (rpm_at_end - 3000.0) * f,
                    max_rpm: redline,
                    speed_ms: 40.0 + 40.0 * f,
                    ..Default::default()
                }
            })
            .collect()
    }

    #[test]
    fn flags_gearing_that_is_too_short() {
        // Sits on the limiter for the last ~1.5 s of a 6 s pull.
        let mut s = lap(6, 8000.0, 8000.0, 6.0);
        for x in s.iter_mut().skip(300) {
            x.rpm = 8000.0;
        }
        let r = analyse(&s);
        assert!(
            matches!(r.verdict, GearingVerdict::TooShort { .. }),
            "verdict was {:?}",
            r.verdict
        );
        assert!(r.headline().contains("Lengthen"));
    }

    #[test]
    fn flags_gearing_that_is_too_long() {
        // Only ever reaches 85% of redline in top.
        let s = lap(6, 6800.0, 8000.0, 6.0);
        let r = analyse(&s);
        assert!(
            matches!(r.verdict, GearingVerdict::TooLong { .. }),
            "verdict was {:?}",
            r.verdict
        );
        assert!(r.headline().contains("Shorten"));
    }

    #[test]
    fn accepts_gearing_that_lands_just_under_redline() {
        let s = lap(6, 7900.0, 8000.0, 6.0);
        let r = analyse(&s);
        assert_eq!(r.verdict, GearingVerdict::WellMatched);
    }

    #[test]
    fn histogram_sums_to_one() {
        let s = lap(4, 7500.0, 8000.0, 4.0);
        let r = analyse(&s);
        let total: f32 = r.rpm_histogram.iter().sum();
        assert!((total - 1.0).abs() < 1e-3, "histogram summed to {total}");
    }

    #[test]
    fn insufficient_data_is_reported_not_guessed() {
        assert_eq!(analyse(&[]).verdict, GearingVerdict::InsufficientData);
        assert_eq!(
            analyse(&lap(3, 5000.0, 8000.0, 0.1)).verdict,
            GearingVerdict::InsufficientData
        );
    }
}
