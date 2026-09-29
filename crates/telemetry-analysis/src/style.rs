//! Driver style fingerprint.
//!
//! Aggregates per-corner metrics into a stable description of *how this
//! particular driver drives*. The point is not to score them — it's that the
//! optimal setup depends on driving style, and two equally fast drivers often
//! need opposite cars.
//!
//! A driver who trail-brakes hard is rotating the car on entry and will be
//! slowed down by a setup that adds entry stability. A driver who brakes in a
//! straight line and turns in on a neutral throttle needs that same stability
//! to be fast. Recommending one car for both is how generic setup guides make
//! people slower.
//!
//! Deliberately not a fitted classifier. These are ~10 interpretable scalars
//! over a handful of corners; k-means over this feature space would already be
//! overkill, and anything heavier would trade away the explainability that is
//! the only reason a driver would act on the advice.

use crate::corner::Corner;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrakeStyle {
    /// Hits the pedal hard and bleeds off — typical of a confident, aggressive
    /// driver, and hard on front tyres.
    Stab,
    /// Progressive build to peak pressure. Kinder to the tyres, slower to
    /// transfer load.
    Squeeze,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationStyle {
    /// Carries brake pressure past turn-in to rotate the car.
    TrailBraker,
    /// Finishes braking in a straight line, turns in neutral.
    StraightLineBraker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerStyle {
    /// Squeezes power on progressively from the apex.
    Progressive,
    /// Waits, then applies a lot of throttle quickly.
    Aggressive,
}

#[derive(Clone, Debug)]
pub struct DriverFingerprint {
    pub corners_analysed: usize,

    pub brake_style: BrakeStyle,
    pub rotation_style: RotationStyle,
    pub power_style: PowerStyle,

    /// Mean peak brake pressure, 0..1.
    pub mean_peak_brake: f32,
    /// Mean rate of brake application, travel/s.
    pub mean_brake_onset_rate: f32,
    /// Mean trail-brake index across corners.
    pub mean_trail_brake: f32,
    /// **Total** coast time per lap, in seconds. The headline improvable number.
    pub total_coast_s: f32,
    /// Mean seconds from apex to throttle.
    pub mean_time_to_throttle_s: f32,
    /// Mean throttle at the apex.
    pub mean_throttle_at_apex: f32,
    /// Mean steering reversals per second — higher means a car that isn't
    /// settled under them.
    pub mean_steering_activity: f32,

    /// Mean balance by phase, radians. Positive = oversteer.
    pub balance_entry: Option<f32>,
    pub balance_apex: Option<f32>,
    pub balance_exit: Option<f32>,
}

// Classification thresholds. These are empirical, chosen so that a clearly
// aggressive input and a clearly progressive one land on opposite sides with
// margin; borderline drivers can fall either way without consequence, because
// the advice layer reads the continuous values, not just the label.
const STAB_ONSET_RATE: f32 = 4.0; // travel/s
const TRAIL_BRAKE_THRESHOLD: f32 = 0.15;
const AGGRESSIVE_RAMP_RATE: f32 = 2.5; // travel/s

impl DriverFingerprint {
    /// A short human-readable characterisation, for the UI header.
    pub fn describe(&self) -> String {
        let brake = match self.brake_style {
            BrakeStyle::Stab => "hits the brake hard",
            BrakeStyle::Squeeze => "squeezes the brake progressively",
        };
        let rot = match self.rotation_style {
            RotationStyle::TrailBraker => "trail-brakes to rotate",
            RotationStyle::StraightLineBraker => "brakes in a straight line",
        };
        let pow = match self.power_style {
            PowerStyle::Progressive => "feeds the power in",
            PowerStyle::Aggressive => "gets on the throttle hard",
        };
        format!("{brake}, {rot}, {pow}")
    }
}

pub fn fingerprint(corners: &[Corner]) -> Option<DriverFingerprint> {
    if corners.is_empty() {
        return None;
    }
    let n = corners.len() as f32;

    let mean = |f: fn(&Corner) -> f32| corners.iter().map(f).sum::<f32>() / n;
    let mean_opt = |f: fn(&Corner) -> Option<f32>| {
        let vals: Vec<f32> = corners.iter().filter_map(f).collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f32>() / vals.len() as f32)
    };

    let mean_peak_brake = mean(|c| c.peak_brake);
    let mean_brake_onset_rate = mean(|c| c.brake_onset_rate);
    let mean_trail_brake = mean(|c| c.trail_brake_index);
    let total_coast_s = corners.iter().map(|c| c.coast_time_s).sum();
    let mean_time_to_throttle_s = mean(|c| c.time_to_throttle_s);
    let mean_throttle_at_apex = mean(|c| c.throttle_at_apex);
    let mean_ramp = mean(|c| c.throttle_ramp_rate);
    let mean_steering_activity = mean(|c| c.steering_reversals_per_s);

    Some(DriverFingerprint {
        corners_analysed: corners.len(),
        brake_style: if mean_brake_onset_rate > STAB_ONSET_RATE {
            BrakeStyle::Stab
        } else {
            BrakeStyle::Squeeze
        },
        rotation_style: if mean_trail_brake > TRAIL_BRAKE_THRESHOLD {
            RotationStyle::TrailBraker
        } else {
            RotationStyle::StraightLineBraker
        },
        power_style: if mean_ramp > AGGRESSIVE_RAMP_RATE {
            PowerStyle::Aggressive
        } else {
            PowerStyle::Progressive
        },
        mean_peak_brake,
        mean_brake_onset_rate,
        mean_trail_brake,
        total_coast_s,
        mean_time_to_throttle_s,
        mean_throttle_at_apex,
        mean_steering_activity,
        balance_entry: mean_opt(|c| c.balance_entry),
        balance_apex: mean_opt(|c| c.balance_apex),
        balance_exit: mean_opt(|c| c.balance_exit),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corner_with(trail: f32, onset: f32, ramp: f32) -> Corner {
        Corner {
            index: 0,
            t_start_s: 0.0,
            t_apex_s: 1.0,
            t_end_s: 2.0,
            entry_speed_ms: 70.0,
            min_speed_ms: 40.0,
            exit_speed_ms: 65.0,
            peak_lat_g: 1.8,
            peak_brake: 0.9,
            brake_onset_rate: onset,
            time_to_peak_brake_s: 0.2,
            trail_brake_index: trail,
            coast_time_s: 0.3,
            time_to_throttle_s: 0.2,
            throttle_ramp_rate: ramp,
            throttle_at_apex: 0.1,
            balance_entry: Some(-0.02),
            balance_apex: Some(0.0),
            balance_exit: Some(0.03),
            steering_reversals_per_s: 1.0,
        }
    }

    #[test]
    fn classifies_an_aggressive_trail_braker() {
        let c = vec![corner_with(0.4, 8.0, 4.0); 4];
        let f = fingerprint(&c).unwrap();
        assert_eq!(f.brake_style, BrakeStyle::Stab);
        assert_eq!(f.rotation_style, RotationStyle::TrailBraker);
        assert_eq!(f.power_style, PowerStyle::Aggressive);
    }

    #[test]
    fn classifies_a_smooth_straight_line_braker() {
        let c = vec![corner_with(0.02, 1.5, 1.0); 4];
        let f = fingerprint(&c).unwrap();
        assert_eq!(f.brake_style, BrakeStyle::Squeeze);
        assert_eq!(f.rotation_style, RotationStyle::StraightLineBraker);
        assert_eq!(f.power_style, PowerStyle::Progressive);
    }

    #[test]
    fn coast_time_accumulates_across_corners() {
        let c = vec![corner_with(0.1, 2.0, 1.0); 5];
        let f = fingerprint(&c).unwrap();
        assert!((f.total_coast_s - 1.5).abs() < 1e-4, "5 × 0.3 s");
    }

    #[test]
    fn no_corners_yields_no_fingerprint() {
        assert!(fingerprint(&[]).is_none());
    }
}
