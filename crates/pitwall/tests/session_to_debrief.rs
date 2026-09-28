//! End-to-end test of the recording pipeline, from wire bytes to the debrief
//! window's draw list.
//!
//! The unit tests cover each stage in isolation. This covers the seams, which
//! is where this kind of pipeline actually breaks: a decoder that populates a
//! field the logger never forwards, a log the analysis cannot segment, a
//! debrief the UI cannot lay out. Every stage here is the real one — the only
//! thing simulated is the simulator.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use telemetry_analysis::debrief::{Debrief, TraceScope};
use telemetry_core::protocols::forza;
use telemetry_core::registry::Registry;
use telemetry_core::ring::SpscRing;
use telemetry_core::sample::TelemetrySample;
use telemetry_core::session::{self, SessionConfig, SessionState};

/// Build a Forza Horizon "Dash" datagram — the real wire format, including the
/// 12-byte shift that makes Horizon packets differ from Motorsport ones.
fn forza_packet(t_ms: u32, rpm: f32, speed: f32, thr: u8, brk: u8, lap: f32, lap_no: u16) -> Vec<u8> {
    // 324 bytes is the Horizon Dash layout.
    let mut p = vec![0u8; 324];
    let put_i32 = |p: &mut Vec<u8>, off: usize, v: i32| {
        p[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    let put_f32 = |p: &mut Vec<u8>, off: usize, v: f32| {
        p[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    let put_u16 = |p: &mut Vec<u8>, off: usize, v: u16| {
        p[off..off + 2].copy_from_slice(&v.to_le_bytes());
    };

    put_i32(&mut p, 0, 1); // IsRaceOn
    p[4..8].copy_from_slice(&t_ms.to_le_bytes());
    put_f32(&mut p, 8, rpm * 1.15); // EngineMaxRpm
    put_f32(&mut p, 12, 900.0); // EngineIdleRpm
    put_f32(&mut p, 16, rpm); // CurrentEngineRpm
    put_f32(&mut p, 20, 0.0); // AccelerationX
    put_f32(&mut p, 24, 0.0);
    put_f32(&mut p, 28, if brk > 30 { -15.2 } else { 3.0 }); // AccelerationZ

    // The Dash block. 232 for Motorsport, +12 for Horizon — the shift that
    // makes a Horizon packet read as garbage against a Motorsport layout.
    const DASH: usize = 232 + 12;
    put_f32(&mut p, DASH + 12, speed); // Speed, m/s
    put_f32(&mut p, DASH + 48, lap); // DistanceTraveled
    put_u16(&mut p, DASH + 68, lap_no); // LapNumber
    p[DASH + 71] = thr; // Accel (throttle)
    p[DASH + 72] = brk; // Brake
    p[DASH + 75] = 5; // Gear
    p
}

/// Four laps of a lap-shaped profile, as Forza would transmit it.
fn wire_session() -> Vec<(u64, Vec<u8>)> {
    let hz = 60u64;
    let per_lap = 20 * hz;
    let mut out = Vec::new();
    let mut t_ns = 0u64;
    let mut dist = 0.0f32;

    for lap in 0..4u16 {
        for i in 0..per_lap {
            let ph = i as f32 / per_lap as f32;
            let (thr, brk, speed) = if ph < 0.40 {
                (255u8, 0u8, 78.0f32)
            } else if ph < 0.48 {
                (0, 235, 50.0)
            } else if ph < 0.66 {
                (60, 0, 34.0)
            } else {
                (255, 0, 64.0)
            };
            dist += speed / hz as f32;
            out.push((
                t_ns,
                forza_packet(
                    (t_ns / 1_000_000) as u32,
                    3000.0 + speed * 40.0,
                    speed,
                    thr,
                    brk,
                    dist,
                    lap + 1,
                ),
            ));
            t_ns += 1_000_000_000 / hz;
        }
    }
    out
}

fn decode_all(wire: &[(u64, Vec<u8>)]) -> Vec<TelemetrySample> {
    let mut reg = Registry::new();
    wire.iter()
        .filter_map(|(t, p)| reg.ingest(p, *t))
        .collect()
}

fn wait_for<T>(mut p: impl FnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = p() {
            return v;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn the_fixture_really_is_forza_wire_format() {
    // If this fails, every other test here is measuring a decoder that silently
    // rejected the input and produced nothing.
    let wire = wire_session();
    let decoded = decode_all(&wire);
    assert_eq!(
        decoded.len(),
        wire.len(),
        "the registry rejected packets the fixture claims are valid"
    );
    assert!(forza::decode(&wire[0].1, 0).is_some());
    let peak = decoded
        .iter()
        .map(|s| s.speed_kmh())
        .fold(0.0f32, f32::max);
    assert!((peak - 78.0 * 3.6).abs() < 1.0, "speed did not survive decode: {peak}");
}

#[test]
fn wire_bytes_become_a_debrief_with_laps_traces_and_kpis() {
    let wire = wire_session();
    let ring = Arc::new(SpscRing::<TelemetrySample>::new(1 << 15));
    let handle = session::spawn(
        Arc::clone(&ring),
        SessionConfig { max_samples: 200_000, window_s: 8.0, drain_batch: 4096 },
    )
    .expect("logger");

    handle.start();
    wait_for(|| (handle.state() == SessionState::Recording).then_some(()));

    // Decode on this thread, exactly as the ingest thread would, and push into
    // the same ring.
    let mut reg = Registry::new();
    let mut pushed = 0usize;
    for (t, p) in &wire {
        if let Some(s) = reg.ingest(p, *t) {
            while !ring.push(s) {
                std::hint::spin_loop();
            }
            pushed += 1;
        }
    }
    assert!(pushed > 4000, "only {pushed} samples decoded");

    wait_for(|| (handle.logged() as usize >= pushed).then_some(()));
    handle.end();

    let log = wait_for(|| handle.take_log());
    assert_eq!(log.len(), pushed, "the logger dropped samples it was handed");
    assert_eq!(
        handle.control().lost_in_transit.load(Ordering::Relaxed),
        0,
        "samples were lost between the threads"
    );

    let d = Debrief::compute(&log.to_vec());
    assert!(d.warning.is_none(), "unexpected warning: {:?}", d.warning);

    // Laps segmented from Forza's own lap counter.
    assert_eq!(d.laps.len(), 4);
    assert_eq!(d.kpi.laps_complete, 2, "only the middle laps are comparable");
    assert!(matches!(d.traces.scope, TraceScope::Lap(_)));

    // KPIs that could only come from the decoded wire values.
    assert!(
        (d.kpi.top_speed_kmh - 78.0 * 3.6).abs() < 2.0,
        "top speed {} km/h",
        d.kpi.top_speed_kmh
    );
    let brake_g = d.kpi.max_brake_g.expect("braking g");
    assert!((brake_g - 1.55).abs() < 0.1, "max braking {brake_g} g");
    assert!(d.kpi.best_lap_s.unwrap() > 15.0 && d.kpi.best_lap_s.unwrap() < 25.0);

    // Throttle was sent as a byte; it must arrive as a 0..1 fraction and reach
    // full scale somewhere on the lap.
    assert!(
        d.traces.throttle.iter().any(|v| *v > 0.99),
        "full throttle never reached the trace"
    );
    assert!(
        d.traces.brake.iter().any(|v| *v > 0.85),
        "the braking zone never reached the trace"
    );
}

#[test]
fn a_saturated_producer_never_blocks_and_never_corrupts_the_log() {
    // The ring is deliberately far too small for the burst. The contract is
    // that the producer is never blocked and the samples that *do* land stay in
    // order — a lossy log, never a scrambled one.
    let ring = Arc::new(SpscRing::<TelemetrySample>::new(64));
    let handle = session::spawn(
        Arc::clone(&ring),
        SessionConfig { max_samples: 100_000, window_s: 8.0, drain_batch: 64 },
    )
    .expect("logger");
    handle.start();
    wait_for(|| (handle.state() == SessionState::Recording).then_some(()));

    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let ring = Arc::clone(&ring);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0u64;
            let t = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let s = TelemetrySample { t_capture_ns: n, ..Default::default() };
                // push() returns false when full; it must never park.
                ring.push(s);
                n += 1;
                assert!(t.elapsed() < Duration::from_secs(20), "producer stalled");
            }
            n
        })
    };

    std::thread::sleep(Duration::from_millis(250));
    stop.store(true, Ordering::Relaxed);
    let produced = producer.join().expect("producer");
    assert!(produced > 10_000, "the producer only managed {produced} pushes");

    handle.end();
    let log = wait_for(|| handle.take_log());
    assert!(!log.is_empty());
    let mut prev = None;
    for s in log.iter() {
        if let Some(p) = prev {
            assert!(s.t_capture_ns > p, "ordering violated: {p} then {}", s.t_capture_ns);
        }
        prev = Some(s.t_capture_ns);
    }
}

#[test]
fn reading_the_chart_stays_cheap_while_the_logger_is_saturated() {
    // What the render thread actually does per frame: one strip() and one
    // take_log(). Both must be wait-free against a logger under load — this is
    // the property the whole two-stage design exists to provide, so it gets a
    // measurement rather than an assertion in a comment.
    let ring = Arc::new(SpscRing::<TelemetrySample>::new(1 << 14));
    let mut handle = session::spawn(Arc::clone(&ring), SessionConfig::default()).expect("logger");
    handle.start();
    wait_for(|| (handle.state() == SessionState::Recording).then_some(()));

    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let ring = Arc::clone(&ring);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..512 {
                    ring.push(TelemetrySample::default());
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };

    // Warm up, then measure.
    for _ in 0..200 {
        let _ = handle.strip();
    }
    let t = Instant::now();
    const FRAMES: u32 = 10_000;
    for _ in 0..FRAMES {
        let _ = handle.strip();
        let _ = handle.take_log();
    }
    let per_frame = t.elapsed() / FRAMES;

    stop.store(true, Ordering::Relaxed);
    feeder.join().expect("feeder");

    // A 240 Hz frame is 4.1 ms. Anything in the microsecond range is noise
    // against that; the bound is set high enough to survive a loaded CI box and
    // still fail loudly if a lock ever appears on this path.
    assert!(
        per_frame < Duration::from_micros(50),
        "reading the chart cost {per_frame:?} per frame"
    );
}
