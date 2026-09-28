//! Heuristic vehicle dynamics engine.
//!
//! Corner segmentation, driver fingerprinting, gearing analysis, the end-of-
//! session debrief, and physics-informed setup recommendations.
//!
//! Nothing in this crate opens a socket or fits a parameter. Everything here is
//! reproducible arithmetic over recorded samples, which is what makes it
//! trustworthy enough to put in front of a driver: the same lap always yields
//! the same verdict, and every verdict can be traced back to the corner that
//! produced it.

pub mod advice;
pub mod corner;
pub mod debrief;
pub mod gearing;
pub mod style;

use telemetry_core::sample::TelemetrySample;

/// Run the full analysis pipeline over one lap's worth of samples.
pub struct LapAnalysis {
    pub corners: Vec<corner::Corner>,
    pub fingerprint: Option<style::DriverFingerprint>,
    pub gearing: gearing::GearingReport,
    pub advice: advice::SetupAdvice,
}

pub fn analyse_lap(samples: &[TelemetrySample]) -> LapAnalysis {
    let corners = corner::segment(samples);
    let fingerprint = style::fingerprint(&corners);
    let gearing = gearing::analyse(samples);
    let advice = match &fingerprint {
        Some(fp) => advice::recommend(&corners, fp, &gearing),
        None => advice::SetupAdvice {
            summary: "No corners detected — drive a lap with the app running.".into(),
            ..Default::default()
        },
    };
    LapAnalysis { corners, fingerprint, gearing, advice }
}
