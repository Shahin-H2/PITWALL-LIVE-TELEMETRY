//! Deterministic telemetry analysis: corner segmentation, driver fingerprint,
//! gearing, and physics-informed setup advice.
//!
//! Nothing in this crate calls a network or a model. Everything here is
//! reproducible arithmetic over recorded samples, which is what makes it
//! trustworthy enough to put in front of a driver.

pub mod advice;
pub mod corner;
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
