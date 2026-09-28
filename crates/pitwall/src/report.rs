//! Headless engineering report from a capture.
//!
//! Two outputs from one pass:
//!
//! * a human-readable report on stdout, for the driver;
//! * a compact JSON document, for external tooling — a spreadsheet, a team's
//!   own database, a setup sheet generator.
//!
//! The JSON is deliberately *small*: extracted features, not raw telemetry.
//! Anything downstream wanting to compare two sessions wants the conclusions
//! and the evidence behind them, not 60 Hz x 90 s of samples it would have to
//! re-derive them from. The raw samples are already in the `.pwtl` capture for
//! anyone who does want them.

use std::fmt::Write as _;
use std::io::Write as _;

use telemetry_analysis::advice::{Confidence, Magnitude};
use telemetry_analysis::{analyse_lap, LapAnalysis};
use telemetry_core::record::Replayer;
use telemetry_core::registry::Registry;
use telemetry_core::sample::{SimId, TelemetrySample};

pub fn analyse_capture(
    path: &str,
    json_out: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut replayer = Replayer::open(path)?;
    let mut registry = Registry::new();
    let mut samples: Vec<TelemetrySample> = Vec::with_capacity(replayer.len());

    while let Some(frame) = replayer.step() {
        if let Some(s) = registry.ingest(&frame.payload, frame.t_ns) {
            samples.push(s);
        }
    }

    let sim = registry.active();
    let (seen, decoded) = registry.stats();

    println!("\n\x1b[1mPITWALL ENGINEERING REPORT\x1b[0m");
    println!("  capture   {path}");
    println!("  simulator {sim}");
    println!(
        "  packets   {seen} seen, {decoded} decoded into {} samples",
        samples.len()
    );
    println!(
        "  duration  {:.1}s\n",
        replayer.duration().as_secs_f32()
    );

    if samples.len() < 64 {
        println!("Not enough decoded samples to analyse. Is the capture from a supported sim?");
        return Ok(());
    }

    let a = analyse_lap(&samples);
    print_human(&a, sim);

    if let Some(out) = json_out {
        let json = to_json(&a, sim, &samples);
        let mut f = std::fs::File::create(out)?;
        f.write_all(json.as_bytes())?;
        println!("\n[json] wrote {} bytes to {out}", json.len());
    }
    Ok(())
}

fn print_human(a: &LapAnalysis, _sim: SimId) {
    println!("\x1b[1mDRIVER\x1b[0m");
    match &a.fingerprint {
        Some(f) => {
            println!("  profile          {}", f.describe());
            println!("  corners analysed {}", f.corners_analysed);
            println!("  peak brake       {:.0}%", f.mean_peak_brake * 100.0);
            println!("  brake onset      {:.1} travel/s", f.mean_brake_onset_rate);
            println!("  trail-brake idx  {:.3}", f.mean_trail_brake);
            println!("  coast per lap    {:.2}s", f.total_coast_s);
            println!("  apex throttle    {:.0}%", f.mean_throttle_at_apex * 100.0);
            if let Some(b) = f.balance_entry {
                println!("  balance entry    {:+.2}°  ({})", b.to_degrees(), word(b));
            }
            if let Some(b) = f.balance_apex {
                println!("  balance apex     {:+.2}°  ({})", b.to_degrees(), word(b));
            }
            if let Some(b) = f.balance_exit {
                println!("  balance exit     {:+.2}°  ({})", b.to_degrees(), word(b));
            }
        }
        None => println!("  no corners detected"),
    }

    println!("\n\x1b[1mGEARING\x1b[0m");
    println!("  {}", a.gearing.headline());
    if a.gearing.redline_rpm > 0.0 {
        println!("  redline          {:.0} rpm", a.gearing.redline_rpm);
        println!("  limiter time     {:.2}s", a.gearing.total_limiter_time_s);
        for g in &a.gearing.per_gear {
            println!(
                "  gear {}  {:>5.1}s  {:>5.0}-{:>5.0} rpm  top {:>3.0} km/h",
                g.gear,
                g.time_s,
                g.min_rpm,
                g.max_rpm,
                g.top_speed_ms * 3.6
            );
        }
    }

    println!("\n\x1b[1mSETUP RECOMMENDATIONS\x1b[0m");
    if a.advice.recommendations.is_empty() {
        println!("  none — balance is neutral and gearing is matched");
    }
    for r in &a.advice.recommendations {
        println!(
            "  {} {:<22} {:<7} {:<6} {}",
            r.direction.arrow(),
            r.parameter.label(),
            format!("{:?}", r.magnitude),
            format!("{:?}", r.confidence),
            r.effect
        );
        println!("      evidence: {}", r.evidence);
    }

    if !a.advice.driving_notes.is_empty() {
        println!("\n\x1b[1mDRIVING NOTES\x1b[0m");
        for n in &a.advice.driving_notes {
            println!("  • {}", n.headline);
            println!("    {}", n.detail);
            if let Some(g) = n.estimated_gain_s {
                println!("    estimated gain: {g:.2}s/lap");
            }
        }
    }

    println!("\n  {}\n", a.advice.summary);
}

fn word(b: f32) -> &'static str {
    if b > 0.02 {
        "oversteer"
    } else if b < -0.02 {
        "understeer"
    } else {
        "neutral"
    }
}

/// Escape a string for embedding in JSON.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

fn num(v: f32) -> String {
    if v.is_finite() {
        format!("{v:.4}")
    } else {
        "null".into()
    }
}

/// Emit the analysis as JSON.
///
/// Hand-written rather than pulled in via serde: this is one document with a
/// fixed shape, emitted once per run, and a derive-macro dependency tree is a
/// poor trade for ~60 lines of `write!`.
fn to_json(a: &LapAnalysis, sim: SimId, samples: &[TelemetrySample]) -> String {
    let mut j = String::with_capacity(4096);
    j.push_str("{\n");
    let _ = writeln!(j, "  \"schema\": \"pitwall.analysis.v1\",");
    let _ = writeln!(j, "  \"simulator\": \"{}\",", esc(sim.name()));
    let _ = writeln!(j, "  \"samples\": {},", samples.len());

    // ---- driver -----------------------------------------------------------
    j.push_str("  \"driver\": ");
    match &a.fingerprint {
        Some(f) => {
            j.push_str("{\n");
            let _ = writeln!(j, "    \"profile\": \"{}\",", esc(&f.describe()));
            let _ = writeln!(j, "    \"corners_analysed\": {},", f.corners_analysed);
            let _ = writeln!(j, "    \"brake_style\": \"{:?}\",", f.brake_style);
            let _ = writeln!(j, "    \"rotation_style\": \"{:?}\",", f.rotation_style);
            let _ = writeln!(j, "    \"power_style\": \"{:?}\",", f.power_style);
            let _ = writeln!(j, "    \"mean_peak_brake\": {},", num(f.mean_peak_brake));
            let _ = writeln!(j, "    \"mean_brake_onset_rate\": {},", num(f.mean_brake_onset_rate));
            let _ = writeln!(j, "    \"trail_brake_index\": {},", num(f.mean_trail_brake));
            let _ = writeln!(j, "    \"total_coast_s\": {},", num(f.total_coast_s));
            let _ = writeln!(j, "    \"mean_time_to_throttle_s\": {},", num(f.mean_time_to_throttle_s));
            let _ = writeln!(j, "    \"mean_throttle_at_apex\": {},", num(f.mean_throttle_at_apex));
            let _ = writeln!(j, "    \"steering_reversals_per_s\": {},", num(f.mean_steering_activity));
            let _ = writeln!(
                j,
                "    \"balance_entry_deg\": {},",
                f.balance_entry.map(|v| num(v.to_degrees())).unwrap_or("null".into())
            );
            let _ = writeln!(
                j,
                "    \"balance_apex_deg\": {},",
                f.balance_apex.map(|v| num(v.to_degrees())).unwrap_or("null".into())
            );
            let _ = writeln!(
                j,
                "    \"balance_exit_deg\": {}",
                f.balance_exit.map(|v| num(v.to_degrees())).unwrap_or("null".into())
            );
            j.push_str("  },\n");
        }
        None => j.push_str("null,\n"),
    }

    // ---- gearing ----------------------------------------------------------
    j.push_str("  \"gearing\": {\n");
    let _ = writeln!(j, "    \"verdict\": \"{:?}\",", a.gearing.verdict);
    let _ = writeln!(j, "    \"headline\": \"{}\",", esc(&a.gearing.headline()));
    let _ = writeln!(j, "    \"redline_rpm\": {},", num(a.gearing.redline_rpm));
    let _ = writeln!(j, "    \"total_limiter_time_s\": {},", num(a.gearing.total_limiter_time_s));
    let _ = writeln!(j, "    \"top_gear_used\": {},", a.gearing.top_gear_used);
    let _ = writeln!(j, "    \"top_gear_peak_fraction\": {},", num(a.gearing.top_gear_peak_fraction));
    j.push_str("    \"per_gear\": [\n");
    for (i, g) in a.gearing.per_gear.iter().enumerate() {
        let comma = if i + 1 < a.gearing.per_gear.len() { "," } else { "" };
        let _ = writeln!(
            j,
            "      {{\"gear\": {}, \"time_s\": {}, \"min_rpm\": {}, \"max_rpm\": {}, \"limiter_s\": {}, \"top_speed_kmh\": {}}}{comma}",
            g.gear, num(g.time_s), num(g.min_rpm), num(g.max_rpm), num(g.limiter_time_s), num(g.top_speed_ms * 3.6)
        );
    }
    j.push_str("    ]\n  },\n");

    // ---- corners ----------------------------------------------------------
    j.push_str("  \"corners\": [\n");
    for (i, c) in a.corners.iter().enumerate() {
        let comma = if i + 1 < a.corners.len() { "," } else { "" };
        let _ = writeln!(
            j,
            "    {{\"index\": {}, \"duration_s\": {}, \"entry_kmh\": {}, \"min_kmh\": {}, \"exit_kmh\": {}, \
             \"peak_lat_g\": {}, \"peak_brake\": {}, \"trail_brake_index\": {}, \"coast_s\": {}, \
             \"time_to_throttle_s\": {}, \"balance_entry_deg\": {}, \"balance_exit_deg\": {}}}{comma}",
            c.index,
            num(c.duration_s()),
            num(c.entry_speed_ms * 3.6),
            num(c.min_speed_ms * 3.6),
            num(c.exit_speed_ms * 3.6),
            num(c.peak_lat_g),
            num(c.peak_brake),
            num(c.trail_brake_index),
            num(c.coast_time_s),
            num(c.time_to_throttle_s),
            c.balance_entry.map(|v| num(v.to_degrees())).unwrap_or("null".into()),
            c.balance_exit.map(|v| num(v.to_degrees())).unwrap_or("null".into()),
        );
    }
    j.push_str("  ],\n");

    // ---- recommendations ---------------------------------------------------
    // Each one carries the measurement that produced it, so a consumer of this
    // document can always show the evidence rather than just the verdict.
    j.push_str("  \"physics_recommendations\": [\n");
    for (i, r) in a.advice.recommendations.iter().enumerate() {
        let comma = if i + 1 < a.advice.recommendations.len() { "," } else { "" };
        let _ = writeln!(
            j,
            "    {{\"parameter\": \"{}\", \"direction\": \"{:?}\", \"magnitude\": \"{}\", \
             \"confidence\": \"{}\", \"effect\": \"{}\", \"evidence\": \"{}\"}}{comma}",
            esc(r.parameter.label()),
            r.direction,
            mag_str(r.magnitude),
            conf_str(r.confidence),
            esc(&r.effect),
            esc(&r.evidence)
        );
    }
    j.push_str("  ],\n");

    j.push_str("  \"driving_notes\": [\n");
    for (i, n) in a.advice.driving_notes.iter().enumerate() {
        let comma = if i + 1 < a.advice.driving_notes.len() { "," } else { "" };
        let _ = writeln!(
            j,
            "    {{\"headline\": \"{}\", \"detail\": \"{}\", \"estimated_gain_s\": {}}}{comma}",
            esc(&n.headline),
            esc(&n.detail),
            n.estimated_gain_s.map(num).unwrap_or("null".into())
        );
    }
    j.push_str("  ],\n");

    let _ = writeln!(j, "  \"summary\": \"{}\"", esc(&a.advice.summary));
    j.push_str("}\n");
    j
}

fn mag_str(m: Magnitude) -> &'static str {
    match m {
        Magnitude::Small => "small",
        Magnitude::Medium => "medium",
        Magnitude::Large => "large",
    }
}

fn conf_str(c: Confidence) -> &'static str {
    match c {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_analysis::analyse_lap;
    use telemetry_core::sample::{FieldMask, Vec3};

    fn synthetic() -> Vec<TelemetrySample> {
        let hz = 60.0f32;
        let mut out = Vec::new();
        let mut t = 0.0f32;
        for lap in 0..2 {
            let _ = lap;
            for i in 0..600 {
                let ph = i as f32 / 600.0;
                let cornering = ph > 0.3 && ph < 0.6;
                out.push(TelemetrySample {
                    t_capture_ns: (t * 1e9) as u64,
                    is_active: true,
                    present: FieldMask::SPEED
                        | FieldMask::THROTTLE
                        | FieldMask::BRAKE
                        | FieldMask::ACCEL
                        | FieldMask::GEAR
                        | FieldMask::RPM
                        | FieldMask::STEER,
                    speed_ms: if cornering { 40.0 } else { 75.0 },
                    throttle: if cornering { 0.2 } else { 1.0 },
                    brake: if ph > 0.25 && ph < 0.35 { 0.9 } else { 0.0 },
                    accel: Vec3::new(if cornering { 15.0 } else { 0.0 }, 0.0, 0.0),
                    gear: if cornering { 3 } else { 6 },
                    rpm: if cornering { 5000.0 } else { 7900.0 },
                    max_rpm: 8000.0,
                    steer: if cornering { 0.35 } else { 0.0 },
                    ..Default::default()
                });
                t += 1.0 / hz;
            }
        }
        out
    }

    #[test]
    fn json_is_well_formed_and_parses() {
        let s = synthetic();
        let a = analyse_lap(&s);
        let j = to_json(&a, SimId::Forza, &s);
        // Balanced braces is a cheap structural check without a JSON crate.
        let opens = j.matches('{').count();
        let closes = j.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in emitted JSON");
        assert_eq!(j.matches('[').count(), j.matches(']').count());
        assert!(j.contains("\"schema\": \"pitwall.analysis.v1\""));
        assert!(j.contains("\"physics_recommendations\""));
        assert!(!j.contains("NaN"), "NaN leaked into JSON");
    }

    #[test]
    fn escaping_handles_quotes_and_control_chars() {
        assert_eq!(esc("a\"b"), "a\\\"b");
        assert_eq!(esc("a\nb"), "a\\nb");
        assert_eq!(esc("a\\b"), "a\\\\b");
        assert!(esc("\u{7}").starts_with("\\u"));
    }

    #[test]
    fn non_finite_numbers_become_null_not_nan() {
        assert_eq!(num(f32::NAN), "null");
        assert_eq!(num(f32::INFINITY), "null");
        assert_eq!(num(1.5), "1.5000");
    }
}
