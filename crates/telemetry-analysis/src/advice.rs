//! Algorithmic setup analyzer: physics-informed setup recommendations.
//!
//! # Why this is a rule layer and not a fitted one
//!
//! A setup change is something the driver makes to a car before they go out on
//! it. A recommendation that is confident, fluent and wrong is worse than no
//! recommendation at all, and a statistical fit over this little data gives you
//! exactly that — plausible numbers with nothing in the output to say which of
//! them are trustworthy.
//!
//! So every recommendation here is produced by an explicit rule over a measured
//! symptom, and carries that measurement with it. It runs offline, instantly,
//! and the same input always gives the same output. When the evidence is weak,
//! the rule says so in [`Confidence`] rather than rounding up to certainty.
//!
//! The causal relationships encoded below are standard vehicle dynamics:
//! stiffening an anti-roll bar transfers more lateral load across that axle
//! and *reduces* its grip; adding wing to an axle adds downforce and drag;
//! more differential locking on power improves traction but resists rotation.

use crate::corner::Corner;
use crate::gearing::{GearingReport, GearingVerdict};
use crate::style::{DriverFingerprint, RotationStyle};

/// A tunable on the car. Not every sim exposes every one of these — the
/// consumer filters by what the active sim actually supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parameter {
    FrontWing,
    RearWing,
    FrontAntiRollBar,
    RearAntiRollBar,
    FrontSpringRate,
    RearSpringRate,
    FrontRideHeight,
    RearRideHeight,
    DifferentialPowerLock,
    DifferentialCoastLock,
    BrakeBias,
    FinalDrive,
    FrontCamber,
    RearCamber,
    FrontTyrePressure,
    RearTyrePressure,
}

impl Parameter {
    pub fn label(self) -> &'static str {
        match self {
            Parameter::FrontWing => "Front wing",
            Parameter::RearWing => "Rear wing",
            Parameter::FrontAntiRollBar => "Front anti-roll bar",
            Parameter::RearAntiRollBar => "Rear anti-roll bar",
            Parameter::FrontSpringRate => "Front spring rate",
            Parameter::RearSpringRate => "Rear spring rate",
            Parameter::FrontRideHeight => "Front ride height",
            Parameter::RearRideHeight => "Rear ride height",
            Parameter::DifferentialPowerLock => "Diff lock (power)",
            Parameter::DifferentialCoastLock => "Diff lock (coast)",
            Parameter::BrakeBias => "Brake bias",
            Parameter::FinalDrive => "Final drive",
            Parameter::FrontCamber => "Front camber",
            Parameter::RearCamber => "Rear camber",
            Parameter::FrontTyrePressure => "Front tyre pressure",
            Parameter::RearTyrePressure => "Rear tyre pressure",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Increase,
    Decrease,
}

impl Direction {
    pub fn arrow(self) -> &'static str {
        match self {
            Direction::Increase => "↑",
            Direction::Decrease => "↓",
        }
    }
}

/// How large a change, in units of "clicks" on a typical setup screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Magnitude {
    Small,
    Medium,
    Large,
}

/// How much we trust this recommendation.
///
/// Reported honestly: `Low` means the symptom was present but weak, or the
/// evidence came from few corners. It travels with the recommendation all the
/// way to the screen, so nothing downstream can silently promote a guess into
/// a fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug)]
pub struct Recommendation {
    pub parameter: Parameter,
    pub direction: Direction,
    pub magnitude: Magnitude,
    pub confidence: Confidence,
    /// What the driver will feel if they make this change.
    pub effect: String,
    /// The measurement that produced this recommendation. Every entry must
    /// cite real numbers — this is what makes the advice auditable.
    pub evidence: String,
}

/// A note about driving rather than the car.
///
/// Kept separate on purpose. When a driver is losing three tenths to coasting,
/// changing the differential will not fix it, and pretending otherwise is how
/// setup advice loses credibility.
#[derive(Clone, Debug)]
pub struct DrivingNote {
    pub headline: String,
    pub detail: String,
    pub estimated_gain_s: Option<f32>,
}

#[derive(Clone, Debug, Default)]
pub struct SetupAdvice {
    pub recommendations: Vec<Recommendation>,
    pub driving_notes: Vec<DrivingNote>,
    pub summary: String,
}

/// Balance beyond this many radians of slip-angle difference is a real
/// handling trait rather than noise. ~1.15°.
const BALANCE_SIGNIFICANT: f32 = 0.02;
/// Beyond this it's a pronounced trait worth a larger change. ~2.9°.
const BALANCE_STRONG: f32 = 0.05;
/// Coast time per lap above which we raise it as a driving note.
const COAST_BUDGET_S: f32 = 0.8;

pub fn recommend(
    corners: &[Corner],
    fingerprint: &DriverFingerprint,
    gearing: &GearingReport,
) -> SetupAdvice {
    let mut out = SetupAdvice::default();

    // Confidence scales with how much evidence we have. Two corners is a
    // hunch; fifteen is a measurement.
    let sample_conf = match corners.len() {
        0..=2 => Confidence::Low,
        3..=7 => Confidence::Medium,
        _ => Confidence::High,
    };

    // ---- entry balance ----------------------------------------------------
    if let Some(entry) = fingerprint.balance_entry {
        if entry < -BALANCE_SIGNIFICANT {
            let strong = entry < -BALANCE_STRONG;
            let mag = if strong { Magnitude::Medium } else { Magnitude::Small };
            let ev = format!(
                "Front slips {:.2}° more than rear on entry, averaged over {} corners",
                -entry.to_degrees(),
                corners.len()
            );
            out.recommendations.push(Recommendation {
                parameter: Parameter::FrontAntiRollBar,
                direction: Direction::Decrease,
                magnitude: mag,
                confidence: sample_conf,
                effect: "Softening the front bar keeps more load on the inside front tyre, adding turn-in grip.".into(),
                evidence: ev.clone(),
            });
            out.recommendations.push(Recommendation {
                parameter: Parameter::FrontWing,
                direction: Direction::Increase,
                magnitude: mag,
                confidence: sample_conf,
                effect: "More front downforce sharpens turn-in at speed, at the cost of some straight-line drag.".into(),
                evidence: ev.clone(),
            });

            // Style modifier: a trail-braker rotates the car with the brake
            // pedal, so brake bias is a more direct lever for them than it is
            // for someone who has finished braking before turn-in.
            if fingerprint.rotation_style == RotationStyle::TrailBraker {
                out.recommendations.push(Recommendation {
                    parameter: Parameter::BrakeBias,
                    direction: Direction::Decrease,
                    magnitude: Magnitude::Small,
                    confidence: sample_conf,
                    effect: "Moving bias rearward lets your trailing brake pressure rotate the car instead of pushing the front.".into(),
                    evidence: format!(
                        "{ev}; you trail-brake (index {:.2}), so bias is a direct rotation lever for your style",
                        fingerprint.mean_trail_brake
                    ),
                });
                out.recommendations.push(Recommendation {
                    parameter: Parameter::DifferentialCoastLock,
                    direction: Direction::Decrease,
                    magnitude: Magnitude::Small,
                    confidence: Confidence::Medium,
                    effect: "Less coast lock frees the rear axle under deceleration, helping the car turn while you're still on the brakes.".into(),
                    evidence: ev,
                });
            }
        } else if entry > BALANCE_SIGNIFICANT {
            let ev = format!(
                "Rear slips {:.2}° more than front on entry, averaged over {} corners",
                entry.to_degrees(),
                corners.len()
            );
            out.recommendations.push(Recommendation {
                parameter: Parameter::RearWing,
                direction: Direction::Increase,
                magnitude: if entry > BALANCE_STRONG { Magnitude::Medium } else { Magnitude::Small },
                confidence: sample_conf,
                effect: "More rear downforce settles the car on turn-in; costs top speed.".into(),
                evidence: ev.clone(),
            });
            out.recommendations.push(Recommendation {
                parameter: Parameter::BrakeBias,
                direction: Direction::Increase,
                magnitude: Magnitude::Small,
                confidence: sample_conf,
                effect: "Bias forward reduces rear lock-up tendency and calms entry.".into(),
                evidence: ev,
            });
        }
    }

    // ---- mid-corner balance ----------------------------------------------
    if let Some(apex) = fingerprint.balance_apex {
        if apex < -BALANCE_SIGNIFICANT {
            out.recommendations.push(Recommendation {
                parameter: Parameter::FrontCamber,
                direction: Direction::Increase,
                magnitude: Magnitude::Small,
                confidence: Confidence::Medium,
                effect: "More negative front camber increases the contact patch at full lateral load, cutting mid-corner push.".into(),
                evidence: format!(
                    "Front slips {:.2}° more than rear at the apex",
                    -apex.to_degrees()
                ),
            });
        }
    }

    // ---- exit balance -----------------------------------------------------
    if let Some(exit) = fingerprint.balance_exit {
        if exit > BALANCE_SIGNIFICANT {
            let strong = exit > BALANCE_STRONG;
            let ev = format!(
                "Rear slips {:.2}° more than front on exit; you reach {:.0}% throttle by the apex",
                exit.to_degrees(),
                fingerprint.mean_throttle_at_apex * 100.0
            );
            out.recommendations.push(Recommendation {
                parameter: Parameter::RearAntiRollBar,
                direction: Direction::Decrease,
                magnitude: if strong { Magnitude::Medium } else { Magnitude::Small },
                confidence: sample_conf,
                effect: "Softening the rear bar keeps load on the inside rear, improving traction out of the corner.".into(),
                evidence: ev.clone(),
            });
            out.recommendations.push(Recommendation {
                parameter: Parameter::DifferentialPowerLock,
                direction: Direction::Decrease,
                magnitude: Magnitude::Small,
                confidence: Confidence::Medium,
                effect: "Less power lock stops the inside wheel dragging the car straight when you pick up the throttle.".into(),
                evidence: ev,
            });
        } else if exit < -BALANCE_SIGNIFICANT {
            out.recommendations.push(Recommendation {
                parameter: Parameter::DifferentialPowerLock,
                direction: Direction::Increase,
                magnitude: Magnitude::Small,
                confidence: Confidence::Medium,
                effect: "More power lock puts both rear wheels to work earlier, though it will resist rotation.".into(),
                evidence: format!(
                    "Front slips {:.2}° more than rear on exit — the car pushes under power",
                    -exit.to_degrees()
                ),
            });
        }
    }

    // ---- gearing ----------------------------------------------------------
    match gearing.verdict {
        GearingVerdict::TooShort { limiter_seconds_x100 } => {
            out.recommendations.push(Recommendation {
                parameter: Parameter::FinalDrive,
                direction: Direction::Decrease,
                magnitude: if limiter_seconds_x100 > 100 { Magnitude::Large } else { Magnitude::Medium },
                confidence: Confidence::High,
                effect: "A longer final drive stops the engine hitting the limiter before the braking zone.".into(),
                evidence: format!(
                    "{:.2}s on the limiter in {}th gear",
                    limiter_seconds_x100 as f32 / 100.0,
                    gearing.top_gear_used
                ),
            });
        }
        GearingVerdict::TooLong { peak_fraction_x100 } => {
            out.recommendations.push(Recommendation {
                parameter: Parameter::FinalDrive,
                direction: Direction::Increase,
                magnitude: Magnitude::Medium,
                confidence: Confidence::High,
                effect: "A shorter final drive keeps the engine in its power band down the straight.".into(),
                evidence: format!(
                    "{}th gear peaks at only {}% of redline",
                    gearing.top_gear_used, peak_fraction_x100
                ),
            });
        }
        _ => {}
    }

    // ---- driving, not setup ----------------------------------------------
    if fingerprint.total_coast_s > COAST_BUDGET_S {
        // Rough conversion: coasting mid-corner costs roughly its own duration
        // in lap time at the margin, discounted because some coast is
        // unavoidable during the pedal transition.
        let gain = (fingerprint.total_coast_s - COAST_BUDGET_S) * 0.6;
        out.driving_notes.push(DrivingNote {
            headline: format!("{:.2}s per lap spent coasting", fingerprint.total_coast_s),
            detail: format!(
                "Across {} corners you average {:.2}s between releasing the brake and picking up the throttle. \
                 No setup change recovers this — it's the transition itself. Work on overlapping brake release \
                 with initial throttle.",
                fingerprint.corners_analysed,
                fingerprint.total_coast_s / fingerprint.corners_analysed.max(1) as f32
            ),
            estimated_gain_s: Some(gain),
        });
    }

    if fingerprint.mean_steering_activity > 3.0 {
        out.driving_notes.push(DrivingNote {
            headline: "High steering correction rate".into(),
            detail: format!(
                "{:.1} direction changes per second in corners. Either the car is unsettled beneath you \
                 or you're over-driving the entry. If the balance figures above look neutral, suspect the latter.",
                fingerprint.mean_steering_activity
            ),
            estimated_gain_s: None,
        });
    }

    // ---- summary ----------------------------------------------------------
    out.recommendations.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then(b.magnitude.cmp(&a.magnitude))
    });

    out.summary = if out.recommendations.is_empty() && out.driving_notes.is_empty() {
        "Balance is neutral and gearing is matched. Nothing to change from this data.".into()
    } else {
        format!(
            "{} setup change(s) and {} driving note(s) from {} corners. Driver profile: {}.",
            out.recommendations.len(),
            out.driving_notes.len(),
            corners.len(),
            fingerprint.describe()
        )
    };

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{BrakeStyle, PowerStyle};

    fn fp(entry: Option<f32>, apex: Option<f32>, exit: Option<f32>, coast: f32) -> DriverFingerprint {
        DriverFingerprint {
            corners_analysed: 10,
            brake_style: BrakeStyle::Squeeze,
            rotation_style: RotationStyle::StraightLineBraker,
            power_style: PowerStyle::Progressive,
            mean_peak_brake: 0.9,
            mean_brake_onset_rate: 2.0,
            mean_trail_brake: 0.05,
            total_coast_s: coast,
            mean_time_to_throttle_s: 0.2,
            mean_throttle_at_apex: 0.1,
            mean_steering_activity: 1.0,
            balance_entry: entry,
            balance_apex: apex,
            balance_exit: exit,
        }
    }

    fn corners(n: usize) -> Vec<Corner> {
        vec![
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
                brake_onset_rate: 2.0,
                time_to_peak_brake_s: 0.2,
                trail_brake_index: 0.05,
                coast_time_s: 0.1,
                time_to_throttle_s: 0.2,
                throttle_ramp_rate: 1.0,
                throttle_at_apex: 0.1,
                balance_entry: Some(0.0),
                balance_apex: Some(0.0),
                balance_exit: Some(0.0),
                steering_reversals_per_s: 1.0,
            };
            n
        ]
    }

    #[test]
    fn entry_understeer_softens_the_front_bar() {
        let a = recommend(&corners(10), &fp(Some(-0.06), None, None, 0.2), &GearingReport::default());
        let r = a
            .recommendations
            .iter()
            .find(|r| r.parameter == Parameter::FrontAntiRollBar)
            .expect("expected a front ARB recommendation");
        assert_eq!(r.direction, Direction::Decrease);
        assert!(r.evidence.contains("°"), "evidence must cite the measurement");
    }

    #[test]
    fn exit_oversteer_softens_the_rear_bar_and_frees_the_diff() {
        let a = recommend(&corners(10), &fp(None, None, Some(0.06), 0.2), &GearingReport::default());
        assert!(a
            .recommendations
            .iter()
            .any(|r| r.parameter == Parameter::RearAntiRollBar && r.direction == Direction::Decrease));
        assert!(a.recommendations.iter().any(|r| r.parameter
            == Parameter::DifferentialPowerLock
            && r.direction == Direction::Decrease));
    }

    #[test]
    fn trail_brakers_get_bias_advice_that_straight_line_brakers_do_not() {
        let mut trail = fp(Some(-0.06), None, None, 0.2);
        trail.rotation_style = RotationStyle::TrailBraker;
        trail.mean_trail_brake = 0.4;
        let a = recommend(&corners(10), &trail, &GearingReport::default());
        assert!(a.recommendations.iter().any(|r| r.parameter == Parameter::BrakeBias));

        let b = recommend(&corners(10), &fp(Some(-0.06), None, None, 0.2), &GearingReport::default());
        assert!(
            !b.recommendations.iter().any(|r| r.parameter == Parameter::BrakeBias),
            "a straight-line braker should not get the trail-brake bias tip"
        );
    }

    #[test]
    fn coasting_is_reported_as_driving_not_setup() {
        let a = recommend(&corners(10), &fp(None, None, None, 2.5), &GearingReport::default());
        assert_eq!(a.driving_notes.len(), 1);
        assert!(a.driving_notes[0].estimated_gain_s.unwrap() > 0.0);
        assert!(
            a.recommendations.is_empty(),
            "coasting must not produce a setup change"
        );
    }

    #[test]
    fn neutral_car_gets_no_changes() {
        let a = recommend(&corners(10), &fp(Some(0.0), Some(0.0), Some(0.0), 0.1), &GearingReport::default());
        assert!(a.recommendations.is_empty());
        assert!(a.summary.contains("Nothing to change"));
    }

    #[test]
    fn confidence_scales_with_evidence() {
        let few = recommend(&corners(2), &fp(Some(-0.06), None, None, 0.1), &GearingReport::default());
        let many = recommend(&corners(15), &fp(Some(-0.06), None, None, 0.1), &GearingReport::default());
        assert_eq!(few.recommendations[0].confidence, Confidence::Low);
        assert_eq!(many.recommendations[0].confidence, Confidence::High);
    }
}
