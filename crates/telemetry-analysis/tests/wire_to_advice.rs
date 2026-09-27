//! End-to-end: raw wire bytes -> decode -> segment -> fingerprint -> advice.
//!
//! The unit tests in each module verify their own layer. This one verifies the
//! *seams*, and specifically the one seam that fails silently: per-wheel array
//! ordering. Forza and Assetto Corsa send `[FL, FR, RL, RR]`; the F1 titles
//! send `[RL, RR, FL, FR]`. Get that permutation wrong and nothing crashes,
//! no test in any single module fails, and the app confidently tells the
//! driver to soften the rear bar when the car actually understeers.
//!
//! So: synthesise a car with a *known* handling trait, push it through the
//! real byte-level decoders, and assert the advice comes out the right way
//! round — for each sim independently.

use telemetry_analysis::advice::{Direction, Parameter};
use telemetry_analysis::analyse_lap;
use telemetry_core::protocols::{assetto, forza};
use telemetry_core::registry::Registry;
use telemetry_core::sample::{SimId, TelemetrySample, Wheel};

const HZ: f32 = 60.0;
const REDLINE: f32 = 7800.0;

/// One instant of a synthetic lap.
struct Frame {
    speed: f32,
    rpm: f32,
    gear: i8,
    throttle: f32,
    brake: f32,
    steer: f32,
    lat_g: f32,
    long_g: f32,
    /// Canonical `[FL, FR, RL, RR]`, radians.
    slip_angle: [f32; 4],
}

/// A lap with a deliberate handling trait.
///
/// `rear_bias > 1.0` means the rear slides more than the front — oversteer.
fn synth_lap(rear_bias: f32) -> Vec<Frame> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<Frame>, speed, thr, brk, lat: f32, steer, gear, rpm| {
        // Front/rear slip scale with lateral load; the bias is what we're testing.
        let base = lat.abs() * 0.030;
        let front = base;
        let rear = base * rear_bias;
        let sign = if lat >= 0.0 { 1.0 } else { -1.0 };
        out.push(Frame {
            speed,
            rpm,
            gear,
            throttle: thr,
            brake: brk,
            steer,
            lat_g: lat,
            long_g: thr * 0.9 - brk * 1.7,
            slip_angle: [sign * front, sign * front, sign * rear, sign * rear],
        });
    };

    // Repeat a straight/brake/corner/exit cycle so the analysis has several
    // corners to average over and reaches High confidence.
    for _ in 0..4 {
        for _ in 0..60 {
            push(&mut out, 78.0, 1.0, 0.0, 0.0, 0.0, 6, 7300.0);
        }
        for i in 0..30 {
            let f = i as f32 / 30.0;
            push(&mut out, 78.0 - 26.0 * f, 0.0, 0.92, 0.0, 0.0, 5, 6800.0);
        }
        for i in 0..36 {
            let f = i as f32 / 36.0;
            push(&mut out, 52.0 - 12.0 * f, 0.0, 0.5 * (1.0 - f), 1.0 + f * 0.9, 0.35, 3, 6200.0);
        }
        for i in 0..48 {
            let f = i as f32 / 48.0;
            push(&mut out, 40.0 + 30.0 * f, f.min(1.0), 0.0, 1.9 * (1.0 - f * 0.7), 0.3 * (1.0 - f), 4, 6600.0);
        }
    }
    out
}

// ---- wire encoders: byte-identical to what the sims actually send ---------

fn encode_forza(f: &Frame, t_ms: u32) -> Vec<u8> {
    let mut b = vec![0u8; 324];
    b[0..4].copy_from_slice(&1i32.to_le_bytes());
    b[4..8].copy_from_slice(&t_ms.to_le_bytes());
    b[8..12].copy_from_slice(&REDLINE.to_le_bytes());
    b[16..20].copy_from_slice(&f.rpm.to_le_bytes());
    b[20..24].copy_from_slice(&(f.lat_g * 9.80665).to_le_bytes());
    b[28..32].copy_from_slice(&(f.long_g * 9.80665).to_le_bytes());
    // Forza is natively [FL, FR, RL, RR] — no permutation.
    for (i, v) in f.slip_angle.iter().enumerate() {
        b[164 + i * 4..168 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    let db = 232 + 12; // Horizon dash base
    b[db + 12..db + 16].copy_from_slice(&f.speed.to_le_bytes());
    b[db + 71] = (f.throttle * 255.0) as u8;
    b[db + 72] = (f.brake * 255.0) as u8;
    b[db + 75] = f.gear as u8;
    b[db + 76] = (f.steer * 127.0) as i8 as u8;
    b
}

fn encode_ac(f: &Frame, t_ms: i32) -> Vec<u8> {
    let mut b = vec![0u8; assetto::RT_CAR_INFO_LEN];
    b[0] = b'a';
    b[16..20].copy_from_slice(&f.speed.to_le_bytes());
    b[32..36].copy_from_slice(&f.lat_g.to_le_bytes());
    b[36..40].copy_from_slice(&f.long_g.to_le_bytes());
    b[40..44].copy_from_slice(&t_ms.to_le_bytes());
    b[56..60].copy_from_slice(&f.throttle.to_le_bytes());
    b[60..64].copy_from_slice(&f.brake.to_le_bytes());
    b[68..72].copy_from_slice(&f.rpm.to_le_bytes());
    b[72..76].copy_from_slice(&f.steer.to_le_bytes());
    b[76..80].copy_from_slice(&((f.gear as i32) + 1).to_le_bytes()); // AC 0=R, 1=N
    // AC is natively [FL, FR, RL, RR].
    for (i, v) in f.slip_angle.iter().enumerate() {
        b[100 + i * 4..104 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    b
}

fn f1_header(packet_id: u8, session_time: f32) -> Vec<u8> {
    let mut h = vec![0u8; 29];
    h[0..2].copy_from_slice(&2025u16.to_le_bytes());
    h[6] = packet_id;
    h[15..19].copy_from_slice(&session_time.to_le_bytes());
    h[27] = 0;
    h
}

fn encode_f1_telemetry(f: &Frame, t: f32) -> Vec<u8> {
    let stride = 60usize;
    let mut b = vec![0u8; 29 + 22 * stride + 3];
    b[..29].copy_from_slice(&f1_header(6, t));
    let o = 29;
    b[o..o + 2].copy_from_slice(&((f.speed * 3.6) as u16).to_le_bytes());
    b[o + 2..o + 6].copy_from_slice(&f.throttle.to_le_bytes());
    b[o + 6..o + 10].copy_from_slice(&f.steer.to_le_bytes());
    b[o + 10..o + 14].copy_from_slice(&f.brake.to_le_bytes());
    b[o + 15] = f.gear as u8;
    b[o + 16..o + 18].copy_from_slice(&(f.rpm as u16).to_le_bytes());
    b
}

fn encode_f1_motion_ex(f: &Frame, t: f32) -> Vec<u8> {
    let mut b = vec![0u8; 29 + 240];
    b[..29].copy_from_slice(&f1_header(13, t));
    let o = 29 + 64; // MX_WHEEL_SLIP_ANGLE
    // *** The permutation under test: F1 transmits [RL, RR, FL, FR]. ***
    let [fl, fr, rl, rr] = f.slip_angle;
    for (i, v) in [rl, rr, fl, fr].iter().enumerate() {
        b[o + i * 4..o + 4 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    b
}

fn encode_f1_motion(f: &Frame, t: f32) -> Vec<u8> {
    let stride = 60usize;
    let mut b = vec![0u8; 29 + 22 * stride];
    b[..29].copy_from_slice(&f1_header(0, t));
    let o = 29;
    b[o + 36..o + 40].copy_from_slice(&f.lat_g.to_le_bytes());
    b[o + 40..o + 44].copy_from_slice(&f.long_g.to_le_bytes());
    b
}

/// Push encoded packets through the real registry and collect samples.
fn decode_all(packets: Vec<(Vec<u8>, u64)>) -> (Vec<TelemetrySample>, SimId) {
    let mut reg = Registry::new();
    let mut out = Vec::new();
    for (p, t_ns) in packets {
        if let Some(s) = reg.ingest(&p, t_ns) {
            out.push(s);
        }
    }
    (out, reg.active())
}

fn t_ns(i: usize) -> u64 {
    (i as f64 / HZ as f64 * 1e9) as u64
}

#[test]
fn forza_oversteering_car_is_told_to_soften_the_rear() {
    let lap = synth_lap(1.9); // rear slides 1.9x the front
    let packets: Vec<_> = lap
        .iter()
        .enumerate()
        .map(|(i, f)| (encode_forza(f, (i as f32 / HZ * 1000.0) as u32), t_ns(i)))
        .collect();

    let (samples, sim) = decode_all(packets);
    assert_eq!(sim, SimId::Forza);
    assert!(samples.len() > 500, "decoded only {}", samples.len());

    let a = analyse_lap(&samples);
    let fp = a.fingerprint.as_ref().expect("fingerprint");
    assert!(
        fp.balance_exit.unwrap() > 0.0,
        "rear-biased car should read as oversteer, got {:?}",
        fp.balance_exit
    );
    assert!(
        a.advice.recommendations.iter().any(|r| r.parameter
            == Parameter::RearAntiRollBar
            && r.direction == Direction::Decrease),
        "expected 'soften rear ARB'; got {:?}",
        a.advice
            .recommendations
            .iter()
            .map(|r| (r.parameter, r.direction))
            .collect::<Vec<_>>()
    );
}

#[test]
fn forza_understeering_car_is_told_the_opposite() {
    let lap = synth_lap(0.5); // front slides more than the rear
    let packets: Vec<_> = lap
        .iter()
        .enumerate()
        .map(|(i, f)| (encode_forza(f, 0), t_ns(i)))
        .collect();
    let (samples, _) = decode_all(packets);
    let a = analyse_lap(&samples);
    let fp = a.fingerprint.as_ref().unwrap();

    assert!(
        fp.balance_entry.unwrap() < 0.0,
        "front-biased car should read as understeer, got {:?}",
        fp.balance_entry
    );
    assert!(
        a.advice.recommendations.iter().any(|r| r.parameter
            == Parameter::FrontAntiRollBar
            && r.direction == Direction::Decrease),
        "expected 'soften front ARB' for understeer"
    );
    assert!(
        !a.advice.recommendations.iter().any(|r| r.parameter
            == Parameter::RearAntiRollBar
            && r.direction == Direction::Decrease),
        "an understeering car must NOT be told to soften the rear"
    );
}

/// The regression guard for the wheel-order permutation.
///
/// The same physical car, encoded in F1's `[RL, RR, FL, FR]` order, must
/// produce the same verdict as the Forza encoding. If the permutation in
/// `from_f1_wheel_order` were removed, this test — and only this test — fails.
#[test]
fn f1_wheel_order_produces_the_same_verdict_as_forza() {
    let lap = synth_lap(1.9);

    let mut f1_packets = Vec::new();
    for (i, f) in lap.iter().enumerate() {
        let t = i as f32 / HZ;
        f1_packets.push((encode_f1_motion(f, t), t_ns(i)));
        f1_packets.push((encode_f1_motion_ex(f, t), t_ns(i)));
        f1_packets.push((encode_f1_telemetry(f, t), t_ns(i)));
    }
    let (f1_samples, sim) = decode_all(f1_packets);
    assert_eq!(sim, SimId::F1_25);
    assert!(f1_samples.len() > 500, "decoded only {}", f1_samples.len());

    // Spot-check the permutation at the sample level before analysing.
    // Pick the most heavily loaded sample: on a straight both axles sit at
    // zero slip and the comparison would be vacuous.
    let loaded = f1_samples
        .iter()
        .max_by(|a, b| a.lat_g().abs().total_cmp(&b.lat_g().abs()))
        .expect("samples");
    let front = loaded.slip_angle[Wheel::FrontLeft.idx()].abs();
    let rear = loaded.slip_angle[Wheel::RearLeft.idx()].abs();
    assert!(
        rear > front,
        "after permutation the rear must carry more slip: front={front}, rear={rear}"
    );

    let a = analyse_lap(&f1_samples);
    assert!(
        a.advice.recommendations.iter().any(|r| r.parameter
            == Parameter::RearAntiRollBar
            && r.direction == Direction::Decrease),
        "F1 encoding of an oversteering car must reach the same conclusion as Forza"
    );
}

#[test]
fn assetto_corsa_reaches_the_same_verdict() {
    let lap = synth_lap(1.9);
    let packets: Vec<_> = lap
        .iter()
        .enumerate()
        .map(|(i, f)| (encode_ac(f, (i as f32 / HZ * 1000.0) as i32), t_ns(i)))
        .collect();
    let (samples, sim) = decode_all(packets);
    assert_eq!(sim, SimId::AssettoCorsa);

    let a = analyse_lap(&samples);
    assert!(
        a.advice.recommendations.iter().any(|r| r.parameter
            == Parameter::RearAntiRollBar
            && r.direction == Direction::Decrease),
        "AC encoding of an oversteering car must reach the same conclusion"
    );
}

/// All three sims, same physical car, same conclusion. This is the property
/// that makes the multi-sim abstraction worth having.
#[test]
fn all_three_sims_agree_on_the_same_car() {
    let lap = synth_lap(1.9);

    let forza: Vec<_> = lap.iter().enumerate().map(|(i, f)| (encode_forza(f, 0), t_ns(i))).collect();
    let ac: Vec<_> = lap.iter().enumerate().map(|(i, f)| (encode_ac(f, 0), t_ns(i))).collect();
    let mut f1 = Vec::new();
    for (i, f) in lap.iter().enumerate() {
        // Motion carries the g-forces the corner segmenter keys off; without
        // it F1 looks like a car that never turns.
        f1.push((encode_f1_motion(f, 0.0), t_ns(i)));
        f1.push((encode_f1_motion_ex(f, 0.0), t_ns(i)));
        f1.push((encode_f1_telemetry(f, 0.0), t_ns(i)));
    }

    let verdicts: Vec<_> = [forza, ac, f1]
        .into_iter()
        .map(|p| {
            let (s, sim) = decode_all(p);
            let a = analyse_lap(&s);
            let oversteer = a
                .fingerprint
                .as_ref()
                .and_then(|f| f.balance_exit)
                .map(|b| b > 0.0)
                .unwrap_or(false);
            (sim, oversteer)
        })
        .collect();

    for (sim, oversteer) in &verdicts {
        assert!(*oversteer, "{sim} disagreed: did not detect oversteer");
    }
    assert_eq!(verdicts.len(), 3);
}
