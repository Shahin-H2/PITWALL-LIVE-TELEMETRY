//! Pitwall — live racing telemetry cluster.
//!
//! Modes:
//!   pitwall                        listen on 0.0.0.0:5000, auto-detect the sim
//!   pitwall --record lap.pwtl      ...and capture raw packets while running
//!   pitwall --replay lap.pwtl      drive the UI from a capture instead of UDP
//!   pitwall --analyse lap.pwtl     headless: print the engineering report
//!   pitwall --analyse lap.pwtl --json out.json   ...and emit it for the AI layer

mod anim;
mod audio;
mod font;
mod gfx;
mod report;
mod ui;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use telemetry_core::ingest::{self, IngestConfig};
use telemetry_core::record::Replayer;
use telemetry_core::registry::Registry;
use telemetry_core::sample::{SimId, TelemetrySample};

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
    --json <FILE>         with --analyse, write structured JSON for the AI layer
    --ac-server <ADDR>    Assetto Corsa host to handshake with, e.g. 192.168.1.20:9996
    --capture <SECS>      headless: record for N seconds, no window, then exit
    --screenshot <PNG>    headless: render one frame to a PNG and exit
    --size <WxH>          resolution for --screenshot (default 1600x900)
    --ignition-at <0..1>  with --screenshot: 0 live, 1 standby, between = transition
    --hover               with --screenshot: render a panel in its hover state
    --no-audio            disable interaction sounds
    -h, --help            this text

KEYS (live/replay):
    TAB   cycle the pinned simulator      P   toggle pin / auto-detect
    ESC   quit
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
    let (sample, sim) = match &args.replay {
        Some(p) => {
            let mut rp = Replayer::open(p)?;
            let mut reg = Registry::new();
            let mut best = TelemetrySample::default();
            // Pick the most interesting frame rather than the first, which is
            // usually a stationary car in the pits.
            while let Some(f) = rp.step() {
                if let Some(s) = reg.ingest(&f.payload, f.t_ns) {
                    if s.rpm_fraction() > best.rpm_fraction() {
                        best = s;
                    }
                }
            }
            (best, reg.active())
        }
        None => (synthetic_sample(), SimId::Forza),
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

    let mut draw = ui::DrawList::default();
    draw.clear();
    ui::build_standby(&mut draw, &ign, w as f32, h as f32, sim, true);
    ui::build(
        &mut draw, &dash, &layout, &ign, &sample, sim, w as f32, h as f32, 60.0, false,
    );

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
        Source::Live { handle, rx }
    };

    let event_loop = EventLoop::new()?;
    // Poll, not Wait: this is a realtime instrument, so we render continuously
    // rather than only when the OS thinks something changed.
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App {
        window: None,
        renderer: None,
        source,
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
                // A click counts as "any key" on the standby screen.
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
        // Escape always quits, even from standby. Everything else ignites.
        if !matches!(key, Key::Named(NamedKey::Escape)) && self.ignition.start() {
            self.audio.play(audio::Sfx::Connect);
            return;
        }
        match key {
            Key::Named(NamedKey::Escape) => event_loop.exit(),
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

    fn pump_source(&mut self) -> bool {
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
                    if let Some(s) = registry.ingest(&frame.payload, frame.t_ns) {
                        *current = s;
                        got = true;
                    }
                }
                self.sample = *current;
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

        let Some(renderer) = &mut self.renderer else { return };
        let (w, h) = renderer.size();

        self.ignition.update(dt);
        self.layout = ui::Layout::new(w, h);

        // Hover is only meaningful once the dashboard exists.
        self.hovered = if self.ignition.phase == ui::Phase::Live {
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

        self.draw.clear();
        ui::build_standby(&mut self.draw, &self.ignition, w, h, sim, connected);
        ui::build(
            &mut self.draw,
            &self.dash,
            &self.layout,
            &self.ignition,
            &self.sample,
            sim,
            w,
            h,
            self.pkt_rate,
            self.pinned,
        );

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

        renderer.render(&u, &self.draw.instances);
    }
}
