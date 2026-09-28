//! Pitwall — live racing telemetry cluster.
//!
//! Modes:
//!   pitwall                        listen on 0.0.0.0:5000, auto-detect the sim
//!   pitwall --record lap.pwtl      ...and capture raw packets while running
//!   pitwall --replay lap.pwtl      drive the UI from a capture instead of UDP
//!   pitwall --analyse lap.pwtl     headless: print the engineering report
//!   pitwall --analyse lap.pwtl --json out.json   ...and export it as JSON

mod anim;
mod audio;
mod debrief;
mod font;
mod gfx;
mod report;
mod ui;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use telemetry_analysis::debrief::Debrief;
use telemetry_core::ingest::{self, IngestConfig};
use telemetry_core::record::Replayer;
use telemetry_core::registry::Registry;
use telemetry_core::sample::{SimId, TelemetrySample};
use telemetry_core::session::{self, SessionState, StripFrame};

use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

struct Args {
    port: u16,
    record: Option<String>,
    replay: Option<String>,
    analyse: Option<String>,
    json_out: Option<String>,
    ac_server: Option<String>,
    audio: bool,
    capture_secs: Option<f32>,
    screenshot: Option<String>,
    size: (u32, u32),
    /// 0 = live dashboard, 1 = standby screen, between = mid-transition.
    ignition_at: f32,
    hover: bool,
    /// Render the end-of-session debrief window instead of the bare cluster.
    debrief: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        port: 5000,
        record: None,
        replay: None,
        analyse: None,
        json_out: None,
        ac_server: None,
        audio: true,
        capture_secs: None,
        screenshot: None,
        size: (1600, 900),
        ignition_at: 0.0,
        hover: false,
        debrief: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--port" => a.port = it.next().and_then(|v| v.parse().ok()).unwrap_or(5000),
            "--record" => a.record = it.next(),
            "--replay" => a.replay = it.next(),
            "--analyse" | "--analyze" => a.analyse = it.next(),
            "--json" => a.json_out = it.next(),
            "--ac-server" => a.ac_server = it.next(),
            "--no-audio" => a.audio = false,
            "--capture" => a.capture_secs = it.next().and_then(|v| v.parse().ok()),
            "--screenshot" => a.screenshot = it.next(),
            "--ignition-at" => a.ignition_at = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            "--hover" => a.hover = true,
            "--debrief" => a.debrief = true,
            "--size" => {
                if let Some(v) = it.next() {
                    if let Some((w, h)) = v.split_once(['x', 'X']) {
                        if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                            a.size = (w, h);
                        }
                    }
                }
            }
            "--help" | "-h" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            other => eprintln!("[args] ignoring unknown flag {other:?}"),
        }
    }
    a
}

const HELP: &str = "\
pitwall — live racing telemetry

USAGE:
    pitwall [OPTIONS]

OPTIONS:
    --port <N>            UDP port to listen on (default 5000)
    --record <FILE>       capture raw packets to a .pwtl file
    --replay <FILE>       drive the UI from a capture instead of the network
    --analyse <FILE>      headless engineering report from a capture
    --json <FILE>         with --analyse, export the report as JSON
    --ac-server <ADDR>    Assetto Corsa host to handshake with, e.g. 192.168.1.20:9996
    --capture <SECS>      headless: record for N seconds, no window, then exit
    --screenshot <PNG>    headless: render one frame to a PNG and exit
    --size <WxH>          resolution for --screenshot (default 1600x900)
    --ignition-at <0..1>  with --screenshot: 0 live, 1 standby, between = transition
    --hover               with --screenshot: render a panel in its hover state
    --debrief             with --screenshot: render the end-of-session debrief
    --no-audio            disable interaction sounds
    -h, --help            this text

KEYS (live/replay):
    any   leave the standby screen and start the cluster
    TAB   cycle the pinned simulator      P   toggle pin / auto-detect
    R     start a new recording session   E   end the session and debrief
    ESC   close the debrief, or quit
";

fn main() {
    let args = parse_args();

    // ---- headless analysis ------------------------------------------------
    if let Some(path) = &args.analyse {
        match report::analyse_capture(path, args.json_out.as_deref()) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // ---- headless screenshot ---------------------------------------------
    if let Some(path) = args.screenshot.clone() {
        match run_screenshot(&args, &path) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // ---- headless capture -------------------------------------------------
    // No window, no GPU. For recording on the machine running the sim, or in
    // CI, where opening a surface is neither possible nor wanted.
    if let Some(secs) = args.capture_secs {
        match run_capture(&args, secs) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    if let Err(e) = run_ui(args) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

/// Render a single frame offscreen and write it as a PNG.
///
/// This exists so the UI can be *looked at* without a display — for visual
/// regression in CI, for reviewing a layout change in a diff, and because
/// claims like "the typography is sharp" should be verifiable rather than
/// asserted.
fn run_screenshot(args: &Args, path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (w, h) = args.size;
    let mut renderer = pollster::block_on(gfx::Renderer::new_headless(w, h))
        .map_err(|e| format!("headless renderer: {e}"))?;

    // Source a plausible car state: a real capture if one was given, otherwise
    // a synthetic mid-corner sample so every gauge has something to show.
    let (sample, sim, history) = match &args.replay {
        Some(p) => {
            let mut rp = Replayer::open(p)?;
            let mut reg = Registry::new();
            let mut best = TelemetrySample::default();
            let mut all = Vec::new();
            // Pick the most interesting frame rather than the first, which is
            // usually a stationary car in the pits.
            while let Some(f) = rp.step() {
                if let Some(s) = reg.ingest(&f.payload, f.t_ns) {
                    if s.rpm_fraction() > best.rpm_fraction() {
                        best = s;
                    }
                    all.push(s);
                }
            }
            (best, reg.active(), all)
        }
        None => (synthetic_sample(), SimId::Forza, synthetic_session()),
    };

    let mut dash = ui::Dash::new();
    dash.snap_to(&sample);

    let layout = ui::Layout::new(w as f32, h as f32);
    let mut ign = ui::Ignition::default();
    // Scrub the sequence to wherever the caller asked. Being able to render a
    // specific instant of the transition is what makes it reviewable.
    if args.ignition_at <= 0.0 {
        ign.phase = ui::Phase::Live;
        ign.t = 1.0;
    } else if args.ignition_at >= 1.0 {
        ign.phase = ui::Phase::Standby;
        ign.standby_t = 2.6;
    } else {
        ign.phase = ui::Phase::Igniting;
        ign.t = args.ignition_at;
    }

    let hovered = args.hover.then_some(ui::PanelId::Speed);
    // Let the springs settle so the screenshot shows the resting state.
    for _ in 0..180 {
        dash.update(&sample, 0.0, 1.0 / 60.0, true, hovered, ign.needle_sweep());
    }

    // The strip chart reads a published snapshot, so a screenshot needs one
    // built the same way the logger builds it — otherwise the panel would be
    // the one part of the cluster no picture could ever show.
    //
    // Cut the window just after the last braking event rather than at the end
    // of the recording. A picture of the chart during a flat-out straight is a
    // green rectangle, and says nothing about what the panel is for.
    let strip = session::strip_from_samples(interesting_window(&history), 8.0);

    let input = ui::FrameInput {
        sample: &sample,
        strip: &strip,
        sim,
        pkt_rate: 60.0,
        pinned: false,
        session: SessionState::Recording,
        logged: history.len() as u64,
        lost: 0,
    };

    let mut draw = ui::DrawList::default();
    draw.clear();
    ui::build_standby(&mut draw, &ign, w as f32, h as f32, sim, true);
    ui::build(&mut draw, &dash, &layout, &ign, w as f32, h as f32, &input);

    if args.debrief {
        let modal = debrief::ModalLayout::new(w as f32, h as f32);
        let mut view = debrief::DebriefView::default();
        view.begin();
        view.resolve(Arc::new(Debrief::compute(&history)));
        for _ in 0..180 {
            view.update(1.0 / 60.0, None, &modal);
        }
        debrief::build(&mut draw, &view, &modal, sim.name());
    }

    let u = gfx::Uniforms {
        resolution: [w as f32, h as f32],
        time: 8.0,
        dpi: 1.0,
        rpm_frac: dash.rpm.value,
        throttle: dash.throttle.value,
        brake: dash.brake.value,
        steer: sample.steer,
        speed_norm: (sample.speed_kmh() / 340.0).clamp(0.0, 1.0),
        gear: sample.gear as f32,
        lat_g: dash.lat_g.value,
        long_g: dash.long_g.value,
        balance: dash.balance.value(),
        connected: 1.0,
        sim_id: sim as u8 as f32,
        frame_ms: 16.6,
        limiter: dash.limiter.value(),
        shift_pulse: 0.5,
        slip: 0.0,
        _pad: 0.0,
    };

    renderer.render(&u, &draw.instances);
    let (pw, ph, rgba) = renderer
        .read_pixels()
        .map_err(|e| format!("read_pixels: {e}"))?;

    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), pw, ph);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgba)?;

    println!("[screenshot] wrote {path} ({pw}x{ph}, {} instances)", draw.instances.len());
    Ok(())
}

/// The tail of a recording, ending shortly after the last braking event.
///
/// Used only by the screenshot path, so the strip chart is captured showing a
/// brake application and its release rather than a straight.
fn interesting_window(samples: &[TelemetrySample]) -> &[TelemetrySample] {
    let Some(last_brake) = samples.iter().rposition(|s| s.brake > 0.5) else {
        return samples;
    };
    // Roughly a second and a half of run-off after the release, so the trailing
    // edge of the brake trace is inside the frame rather than against it.
    let after = samples
        .get(last_brake)
        .map(|s| s.t_capture_ns + 1_500_000_000)
        .unwrap_or(u64::MAX);
    let end = samples
        .iter()
        .position(|s| s.t_capture_ns > after)
        .unwrap_or(samples.len());
    &samples[..end.max(last_brake + 1)]
}

/// A car mid-corner, for screenshots with no capture to hand.
fn synthetic_sample() -> TelemetrySample {
    use telemetry_core::sample::{FieldMask, Vec3};
    TelemetrySample {
        sim: SimId::Forza,
        is_active: true,
        present: FieldMask::RPM
            | FieldMask::GEAR
            | FieldMask::THROTTLE
            | FieldMask::BRAKE
            | FieldMask::SPEED
            | FieldMask::ACCEL
            | FieldMask::STEER
            | FieldMask::SLIP_ANGLE,
        rpm: 6820.0,
        max_rpm: 7800.0,
        gear: 4,
        throttle: 0.62,
        brake: 0.0,
        steer: 0.28,
        speed_ms: 58.3,
        accel: Vec3::new(1.35 * 9.80665, 0.0, 0.55 * 9.80665),
        slip_angle: [0.021, 0.021, 0.034, 0.034],
        ..Default::default()
    }
}

/// Four laps of a plausible circuit, for the screenshot and debrief paths.
///
/// Synthetic rather than a bundled capture because it has to exercise every
/// channel the debrief reads — lap markers, distance, accelerometer — and no
/// single real capture from the three supported sims carries all of them.
fn synthetic_session() -> Vec<TelemetrySample> {
    use telemetry_core::sample::{FieldMask, Vec3};
    let hz = 120.0f32;
    let per_lap = (hz * 26.0) as usize;
    let mut out = Vec::with_capacity(per_lap * 4);
    let mut t = 0.0f64;

    for lap in 0..4u16 {
        // Lap 2 is the quick one, so the debrief has a best lap to find.
        let pace = if lap == 2 { 1.035 } else { 1.0 };
        let mut dist = 0.0f32;
        for i in 0..per_lap {
            let ph = i as f32 / per_lap as f32;
            // A lap shaped like a real one: straight, braking zone, slow
            // corner, exit, sweeper, back onto the straight.
            let (thr, brk, speed, steer) = if ph < 0.26 {
                (1.0, 0.0, 82.0, 0.0)
            } else if ph < 0.32 {
                // Braking, with the pedal trailed off rather than stepped off.
                let k = (ph - 0.26) / 0.06;
                (0.0, (1.0 - k * 0.75).max(0.1), 82.0 - k * 40.0, k * 0.2)
            } else if ph < 0.46 {
                (0.22, 0.0, 40.0, 0.5)
            } else if ph < 0.62 {
                let k = (ph - 0.46) / 0.16;
                (0.35 + k * 0.65, 0.0, 40.0 + k * 34.0, 0.5 - k * 0.45)
            } else if ph < 0.80 {
                (0.85, 0.0, 68.0, 0.22)
            } else {
                (1.0, 0.0, 78.0, 0.02)
            };
            let speed = speed * pace;
            dist += speed / hz;

            // Longitudinal g from the speed profile; lateral from the steering.
            let long_g = if brk > 0.1 { -1.55 } else { 0.35 + thr * 0.55 };
            let lat_g = steer * 2.4;

            out.push(TelemetrySample {
                t_capture_ns: (t * 1e9) as u64,
                sim: SimId::Forza,
                is_active: true,
                present: FieldMask::RPM
                    | FieldMask::GEAR
                    | FieldMask::THROTTLE
                    | FieldMask::BRAKE
                    | FieldMask::SPEED
                    | FieldMask::ACCEL
                    | FieldMask::STEER
                    | FieldMask::SLIP_ANGLE
                    | FieldMask::LAP_NUMBER
                    | FieldMask::LAP_DISTANCE,
                rpm: 3200.0 + (speed / 82.0) * 4400.0,
                max_rpm: 7800.0,
                gear: if speed > 70.0 {
                    6
                } else if speed > 55.0 {
                    5
                } else if speed > 45.0 {
                    4
                } else {
                    3
                },
                throttle: thr,
                brake: brk,
                steer,
                speed_ms: speed,
                accel: Vec3::new(lat_g * 9.80665, 0.0, long_g * 9.80665),
                slip_angle: [
                    steer.abs() * 0.05,
                    steer.abs() * 0.05,
                    steer.abs() * 0.07,
                    steer.abs() * 0.07,
                ],
                lap_number: lap + 1,
                lap_distance: dist,
                ..Default::default()
            });
            t += 1.0 / hz as f64;
        }
    }
    out
}

fn run_capture(args: &Args, secs: f32) -> Result<(), Box<dyn std::error::Error>> {
    let path = args
        .record
        .clone()
        .unwrap_or_else(|| "captures/session.pwtl".to_string());
    if let Some(dir) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(dir).ok();
    }

    let cfg = IngestConfig {
        bind: format!("0.0.0.0:{}", args.port).parse()?,
        ac_server: args.ac_server.as_ref().and_then(|s| s.parse().ok()),
        record_to: Some(path.clone().into()),
        history_capacity: 1 << 18,
    };
    println!("[capture] listening on {} for {secs:.1}s -> {path}", cfg.bind);
    let (handle, _rx) = ingest::spawn(cfg)?;

    let start = Instant::now();
    let mut last_report = Instant::now();
    while start.elapsed().as_secs_f32() < secs {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            println!(
                "  {:5.1}s  {} packets  {} samples  sim={}",
                start.elapsed().as_secs_f32(),
                handle.stats.packets.load(Ordering::Relaxed),
                handle.stats.samples.load(Ordering::Relaxed),
                handle.active_sim(),
            );
            last_report = Instant::now();
        }
    }

    let packets = handle.stats.packets.load(Ordering::Relaxed);
    let samples = handle.stats.samples.load(Ordering::Relaxed);
    let max_gap = handle.stats.max_gap_us.load(Ordering::Relaxed);
    let sim = handle.active_sim();
    handle.stop();

    println!("\n[capture] done: {packets} packets, {samples} samples, sim={sim}");
    println!("[capture] worst inter-packet gap: {:.2} ms", max_gap as f64 / 1000.0);
    if packets == 0 {
        println!("[capture] nothing arrived — is the sim transmitting to this port?");
    } else {
        println!("[capture] analyse it:  pitwall --analyse {path}");
    }
    Ok(())
}

/// Where the samples the UI renders are coming from.
enum Source {
    Live {
        handle: ingest::IngestHandle,
        rx: telemetry_core::ring::Consumer<TelemetrySample>,
    },
    Replay {
        replayer: Replayer,
        registry: Registry,
        started: Instant,
        current: TelemetrySample,
        finished: bool,
    },
}

impl Source {
    fn sim(&self) -> SimId {
        match self {
            Source::Live { handle, .. } => handle.active_sim(),
            Source::Replay { registry, .. } => registry.active(),
        }
    }
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<gfx::Renderer>,
    source: Source,
    /// The ring the logger thread drains.
    ///
    /// On a live session the ingest thread fills it. On replay *this* thread
    /// fills it with the samples the replayer decodes, so the logger, the strip
    /// chart and the debrief all run against the same pipeline either way — a
    /// capture is then a faithful rehearsal of a live session rather than a
    /// separate code path that happens to look similar.
    feed: Arc<telemetry_core::ring::SpscRing<TelemetrySample>>,
    session: session::SessionHandle,
    strip: StripFrame,
    /// Set once the dashboard goes live, so a session covers the driving and
    /// not the two seconds of boot animation in front of it.
    armed: bool,
    view: debrief::DebriefView,
    modal: debrief::ModalLayout,
    /// The worker computing a debrief. Polled, never waited on.
    debrief_job: Option<std::sync::mpsc::Receiver<Debrief>>,
    dash: ui::Dash,
    draw: ui::DrawList,
    ignition: ui::Ignition,
    layout: ui::Layout,
    cursor: Option<(f32, f32)>,
    hovered: Option<ui::PanelId>,
    audio: audio::Audio,
    start: Instant,
    last_frame: Instant,
    sample: TelemetrySample,
    last_gear: i8,
    was_connected: bool,
    was_on_limiter: bool,
    pinned: bool,
    pin_cursor: usize,
    packets_at_last_tick: u64,
    last_rate_tick: Instant,
    pkt_rate: f32,
    last_hovered: Option<ui::PanelId>,
}

const PINNABLE: [SimId; 3] = [SimId::Forza, SimId::F1_25, SimId::AssettoCorsa];

fn run_ui(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut feed: Option<Arc<telemetry_core::ring::SpscRing<TelemetrySample>>> = None;
    let source = if let Some(path) = &args.replay {
        let replayer = Replayer::open(path)?;
        println!(
            "[replay] {} frames spanning {:.1}s from {path}",
            replayer.len(),
            replayer.duration().as_secs_f32()
        );
        Source::Replay {
            replayer,
            registry: Registry::new(),
            started: Instant::now(),
            current: TelemetrySample::default(),
            finished: false,
        }
    } else {
        let cfg = IngestConfig {
            bind: format!("0.0.0.0:{}", args.port).parse()?,
            ac_server: args.ac_server.as_ref().and_then(|s| s.parse().ok()),
            record_to: args.record.as_ref().map(Into::into),
            history_capacity: 1 << 17,
        };
        println!("[net] listening on {}", cfg.bind);
        if let Some(ac) = cfg.ac_server {
            println!("[net] will handshake with Assetto Corsa at {ac}");
        }
        if let Some(p) = &cfg.record_to {
            println!("[rec] capturing to {}", p.display());
        }
        let (handle, rx) = ingest::spawn(cfg)?;
        feed = Some(Arc::clone(&handle.history));
        Source::Live { handle, rx }
    };

    // Replay has no ingest thread, so it needs a ring of its own to push into.
    // Sized for a couple of seconds of backlog at 300 Hz.
    let feed = feed.unwrap_or_else(|| {
        Arc::new(telemetry_core::ring::SpscRing::<TelemetrySample>::new(1 << 13))
    });
    let session = session::spawn(Arc::clone(&feed), session::SessionConfig::default())?;
    println!(
        "[log] session logger armed: {} sample ceiling, {:.0}s pedal window",
        session::SessionConfig::default().max_samples,
        session::SessionConfig::default().window_s,
    );

    let event_loop = EventLoop::new()?;
    // Poll, not Wait: this is a realtime instrument, so we render continuously
    // rather than only when the OS thinks something changed.
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        window: None,
        renderer: None,
        source,
        feed,
        session,
        strip: StripFrame::default(),
        armed: false,
        view: debrief::DebriefView::default(),
        modal: debrief::ModalLayout::new(1600.0, 900.0),
        debrief_job: None,
        dash: ui::Dash::new(),
        draw: ui::DrawList::default(),
        ignition: ui::Ignition::default(),
        layout: ui::Layout::new(1600.0, 900.0),
        cursor: None,
        hovered: None,
        audio: if args.audio {
            audio::Audio::new(0.35)
        } else {
            audio::Audio::disabled()
        },
        start: Instant::now(),
        last_frame: Instant::now(),
        sample: TelemetrySample::default(),
        last_gear: 0,
        was_connected: false,
        was_on_limiter: false,
        pinned: false,
        pin_cursor: 0,
        packets_at_last_tick: 0,
        last_rate_tick: Instant::now(),
        pkt_rate: 0.0,
        last_hovered: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Pitwall — Live Telemetry")
            .with_inner_size(winit::dpi::LogicalSize::new(1600.0, 900.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("fatal: could not create window: {e}");
                event_loop.exit();
                return;
            }
        };
        match pollster::block_on(gfx::Renderer::new(window.clone())) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => {
                eprintln!("fatal: renderer init failed: {e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x as f32, position.y as f32));
            }
            WindowEvent::CursorLeft { .. } => self.cursor = None,
            WindowEvent::MouseInput { state: ElementState::Pressed, .. } => {
                // The modal is on top, so it gets first refusal on every click.
                if let Some((cx, cy)) = self.cursor {
                    if self.view.click(cx, cy, &self.modal) {
                        self.audio.play(audio::Sfx::Click);
                        return;
                    }
                }
                // Otherwise a click counts as "any key" on the standby screen.
                if self.ignition.start() {
                    self.audio.play(audio::Sfx::Connect);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => self.on_key(logical_key, event_loop),
            WindowEvent::RedrawRequested => self.frame(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl App {
    fn on_key(&mut self, key: Key, event_loop: &ActiveEventLoop) {
        // Escape is the only key that does not ignite, because it also has to
        // work as "get me out of here" from every later state.
        if !matches!(key, Key::Named(NamedKey::Escape)) && self.ignition.start() {
            self.audio.play(audio::Sfx::Connect);
            return;
        }
        // While the debrief is up it owns the keyboard, so Escape dismisses it
        // rather than quitting the app out from under the driver.
        if self.view.is_open() {
            match key {
                Key::Named(NamedKey::Escape) => {
                    self.view.close();
                    self.audio.play(audio::Sfx::Click);
                    return;
                }
                Key::Character(ref c) if c.eq_ignore_ascii_case("r") => {
                    self.view.close();
                    self.new_session();
                    return;
                }
                _ => return,
            }
        }
        match key {
            Key::Named(NamedKey::Escape) => event_loop.exit(),
            Key::Character(ref c) if c.eq_ignore_ascii_case("r") => self.new_session(),
            Key::Character(ref c) if c.eq_ignore_ascii_case("e") => self.end_session(),
            Key::Named(NamedKey::Tab) => {
                self.pin_cursor = (self.pin_cursor + 1) % PINNABLE.len();
                self.pinned = true;
                self.audio.play(audio::Sfx::Click);
                println!("[ui] pinned to {}", PINNABLE[self.pin_cursor]);
                // Pinning only takes effect on the replay registry here; the
                // live ingest thread owns its own registry, so this is a
                // display-level hint until the control channel is wired.
                if let Source::Replay { registry, .. } = &mut self.source {
                    registry.pin(PINNABLE[self.pin_cursor]);
                }
            }
            Key::Character(ref c) if c.eq_ignore_ascii_case("p") => {
                self.pinned = !self.pinned;
                self.audio.play(audio::Sfx::Click);
                if let Source::Replay { registry, .. } = &mut self.source {
                    if self.pinned {
                        registry.pin(PINNABLE[self.pin_cursor]);
                    } else {
                        registry.unpin();
                    }
                }
                println!("[ui] detection: {}", if self.pinned { "PINNED" } else { "AUTO" });
            }
            _ => {}
        }
    }

    /// Start recording, discarding whatever came before.
    fn new_session(&mut self) {
        self.session.start();
        self.view.data = None;
        self.view.computing = false;
        self.debrief_job = None;
        self.audio.play(audio::Sfx::Connect);
        println!("[log] recording started");
    }

    /// Stop recording and open the debrief over the cluster.
    ///
    /// The analysis itself is handed to a worker thread. Even at a few tens of
    /// milliseconds it has no business on the render thread: the modal is
    /// animating open while it runs, and a stutter in that animation is exactly
    /// the kind of thing that makes software feel cheap.
    fn end_session(&mut self) {
        if self.session.state() != SessionState::Recording {
            println!("[log] no session is running");
            return;
        }
        self.session.end();
        self.view.begin();
        self.audio.play(audio::Sfx::Click);
        println!("[log] session ended, analysing");
    }

    /// Collect a finished log and hand it to a worker, then collect the result.
    fn poll_debrief(&mut self) {
        if self.debrief_job.is_none() {
            if let Some(log) = self.session.take_log() {
                let (tx, rx) = std::sync::mpsc::channel();
                self.debrief_job = Some(rx);
                // `to_vec` flattens the chunked arena; both the copy and the
                // analysis happen here, off the frame path.
                let _ = std::thread::Builder::new()
                    .name("session-debrief".into())
                    .spawn(move || {
                        let d = Debrief::compute(&log.to_vec());
                        let _ = tx.send(d);
                    });
            }
        }
        if let Some(rx) = &self.debrief_job {
            // try_recv, so a frame never waits on the analysis.
            match rx.try_recv() {
                Ok(d) => {
                    println!(
                        "[log] debrief ready: {} samples, {} complete lap(s)",
                        d.kpi.samples, d.kpi.laps_complete
                    );
                    self.view.resolve(Arc::new(d));
                    self.debrief_job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    eprintln!("[log] the debrief worker died");
                    self.view.computing = false;
                    self.debrief_job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn pump_source(&mut self) -> bool {
        // This thread's monotonic clock, for restamping replayed samples.
        let now_ns = self.start.elapsed().as_nanos() as u64;
        match &mut self.source {
            Source::Live { rx, .. } => {
                let fresh = rx.has_fresh();
                self.sample = rx.latest();
                fresh
            }
            Source::Replay {
                replayer,
                registry,
                started,
                current,
                finished,
            } => {
                // Feed every frame whose capture timestamp has come due.
                let elapsed_ns = started.elapsed().as_nanos() as u64;
                let mut got = false;
                // Collected rather than pushed inline, because `self.feed`
                // cannot be borrowed while `self.source` is.
                let mut pushed: Vec<TelemetrySample> = Vec::new();
                loop {
                    let due = match replayer.peek_t_ns() {
                        Some(t) => t <= elapsed_ns,
                        None => {
                            if !*finished {
                                println!("[replay] end of capture; looping");
                                *finished = true;
                            }
                            replayer.reset();
                            *started = Instant::now();
                            break;
                        }
                    };
                    if !due {
                        break;
                    }
                    let Some(frame) = replayer.step() else { break };
                    if let Some(mut s) = registry.ingest(&frame.payload, frame.t_ns) {
                        // Restamp onto this thread's clock. A capture's own
                        // timestamps restart at zero every time the replay
                        // loops, and a log full of backwards-running stamps
                        // gives the debrief a nonsense timebase — negative
                        // durations, percentages over 100, laps of zero
                        // seconds.
                        s.t_capture_ns = now_ns;
                        *current = s;
                        got = true;
                        // Same ring the socket would have filled, so the logger
                        // cannot tell a replay from a live session.
                        pushed.push(s);
                    }
                }
                self.sample = *current;
                for s in &pushed {
                    self.feed.push(*s);
                }
                got
            }
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().clamp(1.0 / 1000.0, 0.25);
        self.last_frame = now;
        let t = (now - self.start).as_secs_f64();

        let fresh = self.pump_source();
        if fresh {
            self.dash.on_sample(&self.sample, t);
        }

        // Packet rate, recomputed once a second.
        if let Source::Live { handle, .. } = &self.source {
            if self.last_rate_tick.elapsed().as_secs_f32() >= 1.0 {
                let now_packets = handle.stats.packets.load(Ordering::Relaxed);
                let d = now_packets.saturating_sub(self.packets_at_last_tick);
                self.pkt_rate = d as f32 / self.last_rate_tick.elapsed().as_secs_f32();
                self.packets_at_last_tick = now_packets;
                self.last_rate_tick = Instant::now();
            }
        } else {
            self.pkt_rate = 60.0;
        }

        let sim = self.source.sim();
        let connected = sim != SimId::Unknown && self.sample.is_active;

        // ---- sound triggers ------------------------------------------------
        if self.sample.gear != self.last_gear && self.sample.gear != 0 {
            self.audio.play(audio::Sfx::Shift);
            self.last_gear = self.sample.gear;
        }
        let on_limiter = self.sample.rpm_fraction() > 0.985;
        if on_limiter && !self.was_on_limiter {
            self.audio.play(audio::Sfx::Limiter);
        }
        self.was_on_limiter = on_limiter;
        if connected != self.was_connected {
            self.audio.play(if connected {
                audio::Sfx::Connect
            } else {
                audio::Sfx::Disconnect
            });
            if connected {
                // Arrive at the car's current state rather than sweeping up to
                // it from zero over the first second.
                self.dash.snap_to(&self.sample);
            }
            self.was_connected = connected;
        }

        // Only the surface size is needed until the very end of the frame, and
        // holding a mutable borrow of the renderer across the update would stop
        // the frame from touching anything else on `self`.
        let Some((w, h)) = self.renderer.as_ref().map(|r| r.size()) else { return };

        let was_igniting = self.ignition.phase != ui::Phase::Live;
        self.ignition.update(dt);
        self.layout = ui::Layout::new(w, h);
        self.modal = debrief::ModalLayout::new(w, h);

        // Arm recording the moment the cluster is live. A session should cover
        // the driving, not the boot animation in front of it, and asking the
        // driver to press record before they drive is how sessions get lost.
        if was_igniting && self.ignition.phase == ui::Phase::Live && !self.armed {
            self.armed = true;
            self.session.start();
            println!("[log] recording started");
        }

        // One atomic swap: the newest decimated pedal window.
        self.strip = self.session.strip();
        self.poll_debrief();
        self.view.update(dt, self.cursor, &self.modal);

        // Hover is only meaningful once the dashboard exists, and the modal
        // swallows it while it is up — panels lighting up behind a dialog is
        // the kind of detail that reads as sloppy even when nobody can say why.
        self.hovered = if self.ignition.phase == ui::Phase::Live && !self.view.is_open() {
            self.cursor.and_then(|(cx, cy)| self.layout.hit(cx, cy))
        } else {
            None
        };
        if self.hovered != self.last_hovered {
            if self.hovered.is_some() {
                self.audio.play(audio::Sfx::Click);
            }
            self.last_hovered = self.hovered;
        }

        self.dash.update(
            &self.sample,
            t,
            dt,
            connected,
            self.hovered,
            self.ignition.needle_sweep(),
        );

        let input = ui::FrameInput {
            sample: &self.sample,
            strip: &self.strip,
            sim,
            pkt_rate: self.pkt_rate,
            pinned: self.pinned,
            session: self.session.state(),
            logged: self.session.logged(),
            lost: self.session.control().lost_in_transit.load(Ordering::Relaxed),
        };

        self.draw.clear();
        ui::build_standby(&mut self.draw, &self.ignition, w, h, sim, connected);
        ui::build(&mut self.draw, &self.dash, &self.layout, &self.ignition, w, h, &input);
        if self.view.is_visible() {
            debrief::build(&mut self.draw, &self.view, &self.modal, sim.name());
        }

        let u = gfx::Uniforms {
            resolution: [w, h],
            time: t as f32,
            dpi: self.window.as_ref().map(|w| w.scale_factor() as f32).unwrap_or(1.0),
            rpm_frac: self.dash.rpm.value,
            throttle: self.dash.throttle.value,
            brake: self.dash.brake.value,
            steer: self.sample.steer,
            speed_norm: (self.sample.speed_kmh() / 340.0).clamp(0.0, 1.0),
            gear: self.sample.gear as f32,
            lat_g: self.dash.lat_g.value,
            long_g: self.dash.long_g.value,
            balance: self.dash.balance.value(),
            connected: self.dash.connected.value(),
            sim_id: sim as u8 as f32,
            frame_ms: dt * 1000.0,
            limiter: self.dash.limiter.value(),
            shift_pulse: self.dash.shift_pulse(),
            slip: 0.0,
            _pad: 0.0,
        };

        if let Some(renderer) = &mut self.renderer {
            renderer.render(&u, &self.draw.instances);
        }
    }
}
