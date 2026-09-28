//! The end-of-session debrief window.
//!
//! A modal over the live cluster rather than a separate window, for one
//! practical reason: a second OS window means a second surface, a second swap
//! chain, and a compositor deciding when each of them presents. The dashboard's
//! whole latency argument rests on owning exactly one present path, and a
//! debrief panel is not worth giving that up for.
//!
//! The content follows a trackside debrief rather than a dashboard:
//!
//! * **KPI tiles first.** The handful of numbers that decide whether the run
//!   was any good, large enough to read from behind someone's shoulder.
//! * **Stacked traces, aligned by distance.** Speed, throttle, brake and
//!   longitudinal g on a shared x axis, so a moment in one channel lines up
//!   vertically with the same moment in the others. Stacking rather than
//!   overlaying is deliberate: four channels in one frame with four different
//!   units is a picture, not a measurement.
//!
//! Nothing here computes anything. The [`Debrief`] arrives finished from a
//! worker thread; this module only lays it out.

use std::sync::Arc;

use crate::anim::Spring;
use crate::ui::{col, DrawList, Rect};
use telemetry_analysis::debrief::{lap_time, Debrief, TraceScope, TRACE_BINS};

/// How long the modal takes to open. Short — the driver asked for this, so
/// making them watch an animation is a tax, not theatre.
const OPEN_TAU: f32 = 0.16;

/// Panel state and the animation that gets it on screen.
pub struct DebriefView {
    /// Where the modal is heading: 1 open, 0 closed.
    target: f32,
    open_amt: Spring,
    /// True between "end session" and the analysis landing.
    pub computing: bool,
    pub data: Option<Arc<Debrief>>,
    pub close_hover: Spring,
    /// Which stacked plot the cursor is over, for a subtle highlight.
    pub plot_hover: [Spring; PLOTS],
}

pub const PLOTS: usize = 4;

impl Default for DebriefView {
    fn default() -> Self {
        Self {
            target: 0.0,
            open_amt: Spring::new(0.0, OPEN_TAU).with_damping(0.9),
            computing: false,
            data: None,
            close_hover: Spring::new(0.0, 0.18).with_damping(0.72),
            plot_hover: std::array::from_fn(|_| Spring::new(0.0, 0.2).with_damping(0.8)),
        }
    }
}

impl DebriefView {
    /// Open the window in its "analysing" state.
    pub fn begin(&mut self) {
        self.target = 1.0;
        self.computing = true;
        self.data = None;
    }

    /// Hand over a finished analysis.
    pub fn resolve(&mut self, d: Arc<Debrief>) {
        self.data = Some(d);
        self.computing = false;
    }

    pub fn close(&mut self) {
        self.target = 0.0;
    }

    /// Whether the window is taking input. False the instant it starts closing,
    /// so a click during the fade-out cannot land on a control.
    pub fn is_open(&self) -> bool {
        self.target > 0.5
    }

    /// Whether anything of it is still on screen.
    pub fn is_visible(&self) -> bool {
        self.target > 0.5 || self.open_amt.value > 0.004
    }

    pub fn amount(&self) -> f32 {
        self.open_amt.value.clamp(0.0, 1.0)
    }

    pub fn update(&mut self, dt: f32, cursor: Option<(f32, f32)>, lay: &ModalLayout) {
        self.open_amt.update(self.target, dt);
        let hovering_close = self.is_open()
            && cursor.map(|(x, y)| lay.close.contains(x, y)).unwrap_or(false);
        self.close_hover.update(if hovering_close { 1.0 } else { 0.0 }, dt);
        for i in 0..PLOTS {
            let over = self.is_open()
                && cursor.map(|(x, y)| lay.plots[i].contains(x, y)).unwrap_or(false);
            self.plot_hover[i].update(if over { 1.0 } else { 0.0 }, dt);
        }
    }

    /// Did a click at `(x, y)` hit the close control?
    pub fn click(&mut self, x: f32, y: f32, lay: &ModalLayout) -> bool {
        if self.is_open() && lay.close.contains(x, y) {
            self.close();
            return true;
        }
        false
    }
}

// ============================================================================
// Layout
// ============================================================================

/// Every rectangle in the modal, derived from the window size.
///
/// Separate from the drawing for the same reason the dashboard's layout is: the
/// close button has to be hit-tested against exactly the rectangle it was drawn
/// in, and the only way to guarantee that is for one piece of code to own it.
pub struct ModalLayout {
    pub s: f32,
    pub scrim: Rect,
    pub panel: Rect,
    pub close: Rect,
    /// The KPI tiles, left to right.
    pub tiles: [Rect; TILES],
    pub sector_row: Rect,
    /// Stacked trace plots, top to bottom.
    pub plots: [Rect; PLOTS],
    pub footer_y: f32,
}

pub const TILES: usize = 6;

/// Which channel a stacked plot is showing. Each needs its own vertical scale
/// and its own answer to "does zero sit at the bottom or in the middle".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TraceKind {
    Speed,
    Throttle,
    Brake,
    LongG,
}

impl ModalLayout {
    pub fn new(w: f32, h: f32) -> Self {
        let s = (h / 900.0).clamp(0.55, 2.4);
        let panel = Rect {
            x: w * 0.055,
            y: h * 0.065,
            w: w * 0.89,
            h: h * 0.87,
        };
        let inner = 26.0 * s;
        let close_sz = 30.0 * s;
        let close = Rect {
            x: panel.x + panel.w - inner - close_sz,
            y: panel.y + inner - 4.0 * s,
            w: close_sz,
            h: close_sz,
        };

        let tile_y = panel.y + 64.0 * s;
        let tile_h = 68.0 * s;
        let gap = 10.0 * s;
        let tile_w = (panel.w - inner * 2.0 - gap * (TILES as f32 - 1.0)) / TILES as f32;
        let tiles = std::array::from_fn(|i| Rect {
            x: panel.x + inner + (tile_w + gap) * i as f32,
            y: tile_y,
            w: tile_w,
            h: tile_h,
        });

        let sector_row = Rect {
            x: panel.x + inner,
            y: tile_y + tile_h + 8.0 * s,
            w: panel.w - inner * 2.0,
            h: 22.0 * s,
        };

        let plots_top = sector_row.y + sector_row.h + 10.0 * s;
        let footer_h = 26.0 * s;
        // Room under the last plot for the distance axis. Reserved here rather
        // than allowed to spill, so the labels cannot land on the footer at
        // some window size nobody tested.
        let axis_h = 16.0 * s;
        let plots_h = panel.y + panel.h - inner - footer_h - axis_h - plots_top;
        let plot_gap = 8.0 * s;
        let one = (plots_h - plot_gap * (PLOTS as f32 - 1.0)) / PLOTS as f32;
        let plots = std::array::from_fn(|i| Rect {
            x: panel.x + inner,
            y: plots_top + (one + plot_gap) * i as f32,
            w: panel.w - inner * 2.0,
            h: one.max(10.0),
        });

        Self {
            s,
            scrim: Rect { x: 0.0, y: 0.0, w, h },
            panel,
            close,
            tiles,
            sector_row,
            plots,
            footer_y: panel.y + panel.h - inner - footer_h * 0.5,
        }
    }
}

// ============================================================================
// Drawing
// ============================================================================

/// Label and value for one KPI tile. `None` becomes an em dash, never a zero —
/// a sim that does not publish accelerometer data has no maximum braking force,
/// and printing "0.00 g" for that would be a lie told in a large font.
struct Tile<'a> {
    label: &'a str,
    value: String,
    unit: &'a str,
    color: [f32; 4],
}

fn opt<T: std::fmt::Display>(v: Option<T>, f: impl Fn(T) -> String) -> String {
    // Two hyphens rather than an em dash: the glyph atlas is printable ASCII.
    v.map(f).unwrap_or_else(|| "--".into())
}

pub fn build(dl: &mut DrawList, view: &DebriefView, lay: &ModalLayout, sim_name: &str) {
    let a = view.amount();
    if a <= 0.004 {
        return;
    }
    let s = lay.s;

    // Scrim. The live cluster stays visible underneath but recedes, which keeps
    // the driver oriented — a full blackout would feel like leaving the app.
    dl.fill(
        lay.scrim.x,
        lay.scrim.y,
        lay.scrim.w,
        lay.scrim.h,
        0.0,
        [0.02, 0.03, 0.05, 0.80 * a],
    );

    // The panel rises a little and settles. Everything inside inherits the
    // transform, so the whole window moves as one object.
    let (pcx, pcy) = lay.panel.center();
    dl.set_reveal(a, (0.0, (1.0 - a) * 26.0 * s), 0.985 + 0.015 * a, (pcx, pcy));

    let p = lay.panel;
    // A near-solid backing under the glass. The glass shader refracts the
    // *backdrop* texture, not the interface drawn beneath it, so a translucent
    // panel here would let the tachometer show straight through the traces —
    // which is exactly what a glass panel is not supposed to do. The glass on
    // top still supplies the rim, the refraction at the edges and the inner
    // shadow; it simply no longer has the dashboard to see through.
    dl.fill(p.x, p.y, p.w, p.h, 20.0 * s, [0.055, 0.070, 0.105, 0.965]);
    dl.glass(p.x, p.y, p.w, p.h, 20.0 * s, col::GLASS_DEEP, 0.0);

    // ---- header ------------------------------------------------------------
    dl.text(p.x + 26.0 * s, p.y + 24.0 * s, 20.0 * s, col::TEXT, "SESSION DEBRIEF");

    let close_h = view.close_hover.value.clamp(0.0, 1.2);
    dl.fill(
        lay.close.x,
        lay.close.y,
        lay.close.w,
        lay.close.h,
        8.0 * s,
        [
            0.30 + close_h * 0.35,
            0.14 + close_h * 0.06,
            0.16 + close_h * 0.06,
            0.35 + close_h * 0.45,
        ],
    );
    let (ccx, ccy) = lay.close.center();
    let arm = 7.0 * s;
    let thick = 1.8 * s;
    let cross = col::faded(col::TEXT, 0.75 + close_h * 0.25);
    dl.segment(ccx - arm, ccy - arm, ccx + arm, ccy + arm, thick, cross);
    dl.segment(ccx - arm, ccy + arm, ccx + arm, ccy - arm, thick, cross);

    let Some(d) = view.data.as_deref() else {
        let msg = if view.computing {
            "ANALYSING SESSION..."
        } else {
            "NO SESSION RECORDED"
        };
        dl.text_centered(pcx, pcy - 10.0 * s, 16.0 * s, col::TEXT_DIM, msg);
        if view.computing {
            // A determinate progress bar would be a lie: the analysis is one
            // linear pass and finishes before this is drawn twice. An orbiting
            // dot says "working" without claiming to know how far along it is.
            let r = 34.0 * s;
            dl.ring(pcx, pcy + 44.0 * s, r, 1.2 * s, col::TRACK);
            let ang = view.close_hover.value * 0.0 + view.amount() * 9.0;
            dl.circle(pcx + ang.cos() * r, pcy + 44.0 * s + ang.sin() * r, 4.0 * s, col::ACCENT);
        }
        dl.clear_reveal();
        return;
    };

    let k = &d.kpi;
    let scope = match d.traces.scope {
        TraceScope::Lap(n) => format!("LAP {n}"),
        TraceScope::WholeSession => "WHOLE SESSION".to_string(),
    };
    dl.text(
        p.x + 26.0 * s,
        p.y + 46.0 * s,
        11.0 * s,
        col::TEXT_DIM,
        &format!(
            "{}   {:.0}s   {:.2} KM   {} SAMPLES @ {:.0} HZ   {}   TRACES: {}",
            sim_name.to_uppercase(),
            k.duration_s,
            k.distance_m / 1000.0,
            crate::ui::thousands(k.samples as u64),
            k.rate_hz,
            d.traces.distance_source.label(),
            scope,
        ),
    );

    // ---- KPI tiles ---------------------------------------------------------
    let t2b = d.transitions.throttle_to_brake;
    let clean_laps = match k.laps_complete {
        0 => "NO CLEAN LAP".to_string(),
        1 => "OF 1 CLEAN LAP".to_string(),
        n => format!("OF {n} CLEAN LAPS"),
    };
    let tiles = [
        Tile {
            label: "TOP SPEED",
            value: format!("{:.0}", k.top_speed_kmh),
            unit: "KM/H",
            color: col::TEXT,
        },
        Tile {
            label: "BEST LAP",
            value: opt(k.best_lap_s, lap_time),
            unit: &clean_laps,
            color: col::ACCENT,
        },
        Tile {
            label: "MAX BRAKING",
            value: opt(k.max_brake_g, |g| format!("{g:.2}")),
            unit: "G DECEL",
            color: col::BRAKE,
        },
        Tile {
            label: "THR > BRK",
            value: opt(t2b.map(|d| d.mean_ms), |v| format!("{v:.0}")),
            unit: "MS MEAN COAST",
            color: col::WARN,
        },
        Tile {
            label: "PEAK LATERAL",
            value: opt(k.max_lat_g, |g| format!("{g:.2}")),
            unit: "G",
            color: col::UNDERSTEER,
        },
        Tile {
            label: "FULL THROTTLE",
            value: format!("{:.0}", k.full_throttle_pct),
            unit: "% OF SESSION",
            color: col::THROTTLE,
        },
    ];

    for (i, tile) in tiles.iter().enumerate() {
        let r = lay.tiles[i];
        dl.fill(r.x, r.y, r.w, r.h, 10.0 * s, [1.0, 1.0, 1.0, 0.045]);
        dl.fill(r.x, r.y, 2.5 * s, r.h, 1.2 * s, col::faded(tile.color, 0.75));
        dl.text(r.x + 12.0 * s, r.y + 9.0 * s, 9.5 * s, col::TEXT_DIM, tile.label);
        dl.text(r.x + 12.0 * s, r.y + 24.0 * s, 24.0 * s, tile.color, &tile.value);
        dl.text(r.x + 12.0 * s, r.y + r.h - 15.0 * s, 9.0 * s, col::TEXT_DIM, tile.unit);
    }

    // ---- sector row --------------------------------------------------------
    let r = lay.sector_row;
    let mut sx = r.x;
    let sector_label = |i: usize| match i {
        0 => "S1",
        1 => "S2",
        _ => "S3",
    };
    for i in 0..3 {
        let txt = format!(
            "{} {}",
            sector_label(i),
            opt(k.best_sectors_s[i], |v| format!("{v:.3}"))
        );
        dl.text(sx, r.y, 12.0 * s, col::TEXT_DIM, &txt);
        sx += DrawList::text_width(12.0 * s, &txt) + 26.0 * s;
    }
    if let Some(theo) = k.theoretical_best_s {
        dl.text(
            sx,
            r.y,
            12.0 * s,
            col::ACCENT,
            &format!("THEORETICAL {}", lap_time(theo)),
        );
    }
    let bt = d.transitions.brake_to_throttle;
    let mut right = format!(
        "BRK > THR {} MS",
        opt(bt.map(|d| d.mean_ms), |v| format!("{v:.0}"))
    );
    if d.transitions.overlap_events > 0 {
        right = format!(
            "{}   OVERLAP {}×  {:.2}s",
            right, d.transitions.overlap_events, d.transitions.overlap_total_s
        );
    }
    dl.text_right(
        r.x + r.w,
        r.y,
        12.0 * s,
        if d.transitions.overlap_events > 0 {
            col::OVERLAP
        } else {
            col::TEXT_DIM
        },
        &right,
    );

    // ---- stacked traces ----------------------------------------------------
    // One shared distance axis. Because every plot bins the same distance span
    // into the same 384 columns, a feature at x in one plot is the same point on
    // the circuit as x in every other — which is the entire reason for aligning
    // by distance instead of by time.
    let tr = &d.traces;
    let specs: [(&str, &str, TraceKind); PLOTS] = [
        ("SPEED", "KM/H", TraceKind::Speed),
        ("THROTTLE", "%", TraceKind::Throttle),
        ("BRAKE", "%", TraceKind::Brake),
        ("LONG G", "G", TraceKind::LongG),
    ];

    for (i, (name, unit, kind)) in specs.iter().enumerate() {
        let r = lay.plots[i];
        let hov = view.plot_hover[i].value.clamp(0.0, 1.0);
        dl.fill(r.x, r.y, r.w, r.h, 6.0 * s, [1.0, 1.0, 1.0, 0.03 + hov * 0.035]);

        let (channel, color, signed, top_label): (&[f32; TRACE_BINS], [f32; 4], bool, String) =
            match kind {
                TraceKind::Speed => (
                    &tr.speed_kmh,
                    col::TEXT,
                    false,
                    format!("{:.0}", tr.speed_max),
                ),
                TraceKind::Throttle => (&tr.throttle, col::THROTTLE, false, "100".into()),
                TraceKind::Brake => (&tr.brake, col::BRAKE, false, "100".into()),
                // The font is the 95 printable ASCII glyphs, so "+/-" rather
                // than a plus-minus sign: a character the atlas does not carry
                // would draw as a blank cell.
                TraceKind::LongG => (
                    &tr.long_g,
                    col::WARN,
                    true,
                    format!("+/-{:.1}", tr.g_range),
                ),
            };

        let scale = match kind {
            TraceKind::Speed => tr.speed_max.max(1.0),
            TraceKind::LongG => tr.g_range.max(0.1),
            _ => 1.0,
        };

        let plot = Rect {
            x: r.x + 62.0 * s,
            y: r.y + 6.0 * s,
            w: r.w - 78.0 * s,
            h: r.h - 12.0 * s,
        };
        let baseline = if signed { plot.y + plot.h * 0.5 } else { plot.y + plot.h };
        let amp = if signed { plot.h * 0.5 } else { plot.h };

        dl.fill(plot.x, baseline - 0.5 * s, plot.w, 1.0 * s, 0.0, col::TRACK);
        // Distance gridlines every 500 m — enough to locate a corner without
        // turning the plot into graph paper.
        let step = grid_step(tr.distance_m);
        let mut dm = step;
        while dm < tr.distance_m {
            let x = plot.x + plot.w * (dm / tr.distance_m);
            dl.fill(x - 0.5 * s, plot.y, 1.0 * s, plot.h, 0.0, [1.0, 1.0, 1.0, 0.035]);
            if i == PLOTS - 1 {
                dl.text_centered(
                    x,
                    plot.y + plot.h + 3.0 * s,
                    8.5 * s,
                    col::TEXT_DIM,
                    &format!("{:.0}", dm),
                );
            }
            dm += step;
        }

        dl.text(r.x, r.y + r.h * 0.5 - 9.0 * s, 10.0 * s, col::TEXT_DIM, name);
        dl.text(r.x, r.y + r.h * 0.5 + 2.0 * s, 8.5 * s, col::TEXT_DIM, unit);
        dl.text_right(r.x + r.w, r.y + 4.0 * s, 8.5 * s, col::TEXT_DIM, &top_label);

        // Filled area under the pedal channels — a pedal trace is read as "how
        // much of the time was I on it", and area answers that faster than a
        // line does. Speed and g stay as lines: their area means nothing.
        let filled = matches!(kind, TraceKind::Throttle | TraceKind::Brake);
        let bw = (plot.w / TRACE_BINS as f32).max(0.7);

        // Runs of valid bins, so a gap in the data breaks the line instead of
        // being bridged by a segment that describes nothing.
        let mut run: Vec<(f32, f32)> = Vec::with_capacity(TRACE_BINS);
        let flush = |dl: &mut DrawList, run: &mut Vec<(f32, f32)>| {
            if run.len() >= 2 {
                dl.polyline(run, 1.6 * s, color);
            } else if run.len() == 1 {
                dl.circle(run[0].0, run[0].1, 1.1 * s, color);
            }
            run.clear();
        };

        for b in 0..TRACE_BINS {
            if !tr.valid[b] {
                flush(dl, &mut run);
                continue;
            }
            let x = plot.x + plot.w * (b as f32 / TRACE_BINS as f32);
            let v = (channel[b] / scale).clamp(if signed { -1.0 } else { 0.0 }, 1.0);
            let y = baseline - v * amp;
            if filled {
                dl.fill(x, y, bw, baseline - y, 0.0, col::faded(color, 0.22));
            } else if signed && v.abs() > 0.02 {
                dl.fill(
                    x,
                    y.min(baseline),
                    bw,
                    (baseline - y).abs(),
                    0.0,
                    col::faded(color, 0.14),
                );
            }
            run.push((x + bw * 0.5, y));
        }
        flush(dl, &mut run);
    }

    // ---- footer ------------------------------------------------------------
    let footer = match &d.warning {
        Some(warn) => warn.clone(),
        None => format!(
            "{} COMPLETE LAP(S)   BRAKING {:.0}% OF SESSION   COASTING {:.0}%",
            k.laps_complete, k.braking_pct, k.coasting_pct
        ),
    };
    dl.text(
        p.x + 26.0 * s,
        lay.footer_y - 6.0 * s,
        10.5 * s,
        if d.warning.is_some() { col::WARN } else { col::TEXT_DIM },
        &footer,
    );
    dl.text_right(
        p.x + p.w - 26.0 * s,
        lay.footer_y - 6.0 * s,
        10.5 * s,
        col::TEXT_DIM,
        "ESC CLOSE   R NEW SESSION",
    );

    dl.clear_reveal();
}

/// A round distance interval that yields roughly 6–12 gridlines.
fn grid_step(total_m: f32) -> f32 {
    for step in [50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0] {
        if total_m / step <= 12.0 {
            return step;
        }
    }
    (total_m / 10.0).max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_analysis::debrief::Debrief;
    use telemetry_core::sample::{FieldMask, TelemetrySample, Vec3};

    fn samples() -> Vec<TelemetrySample> {
        let hz = 60.0f32;
        let mut out = Vec::new();
        let mut t = 0.0f64;
        for lap in 0..4u16 {
            let mut d = 0.0f32;
            for i in 0..900 {
                let ph = i as f32 / 900.0;
                let braking = (0.30..0.38).contains(&ph);
                let cornering = (0.38..0.55).contains(&ph);
                let speed = if braking {
                    52.0
                } else if cornering {
                    33.0
                } else {
                    74.0
                };
                d += speed / hz;
                out.push(TelemetrySample {
                    t_capture_ns: (t * 1e9) as u64,
                    is_active: true,
                    present: FieldMask::SPEED
                        | FieldMask::THROTTLE
                        | FieldMask::BRAKE
                        | FieldMask::ACCEL
                        | FieldMask::RPM
                        | FieldMask::LAP_NUMBER
                        | FieldMask::LAP_DISTANCE,
                    speed_ms: speed,
                    throttle: if braking || cornering { 0.0 } else { 1.0 },
                    brake: if braking { 0.94 } else { 0.0 },
                    accel: Vec3::new(
                        if cornering { 13.0 } else { 0.0 },
                        0.0,
                        if braking { -15.0 } else { 2.5 },
                    ),
                    rpm: 6500.0,
                    max_rpm: 8000.0,
                    lap_number: lap + 1,
                    lap_distance: d,
                    ..Default::default()
                });
                t += 1.0 / hz as f64;
            }
        }
        out
    }

    fn resolved() -> DebriefView {
        let mut v = DebriefView::default();
        v.begin();
        v.resolve(Arc::new(Debrief::compute(&samples())));
        v
    }

    fn settle(v: &mut DebriefView, lay: &ModalLayout, cursor: Option<(f32, f32)>) {
        for _ in 0..120 {
            v.update(1.0 / 60.0, cursor, lay);
        }
    }

    #[test]
    fn a_closed_window_draws_nothing() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut dl = DrawList::default();
        build(&mut dl, &DebriefView::default(), &lay, "Forza");
        assert!(dl.instances.is_empty());
    }

    #[test]
    fn opening_before_the_analysis_lands_shows_a_working_state() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = DebriefView::default();
        v.begin();
        settle(&mut v, &lay, None);
        assert!(v.computing);
        let mut dl = DrawList::default();
        build(&mut dl, &v, &lay, "Forza");
        // The frame, the close control and the working indicator, but no plots.
        assert!(dl.instances.len() > 8, "got {}", dl.instances.len());
        assert!(dl.instances.len() < 200, "plots were drawn with no data");
    }

    #[test]
    fn a_resolved_window_draws_all_four_traces() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = resolved();
        settle(&mut v, &lay, None);
        let mut dl = DrawList::default();
        build(&mut dl, &v, &lay, "Forza");
        let segments = dl
            .instances
            .iter()
            .filter(|i| i.meta[0] == crate::ui::SHAPE_SEG)
            .count();
        // Four traces of a few hundred points each, plus the two strokes of the
        // close cross.
        assert!(segments > 4 * 200, "only {segments} trace segments");
        for i in &dl.instances {
            assert!(i.rect.iter().all(|v| v.is_finite()), "non-finite rect {:?}", i.rect);
            assert!(i.rect[2] >= 0.0 && i.rect[3] >= 0.0, "negative size {:?}", i.rect);
            assert!(i.color.iter().all(|v| v.is_finite()));
        }
    }

    #[test]
    fn the_close_control_is_hit_where_it_is_drawn() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = resolved();
        settle(&mut v, &lay, None);
        let (cx, cy) = lay.close.center();
        assert!(v.click(cx, cy, &lay), "the close control missed its own centre");
        assert!(!v.is_open());
    }

    #[test]
    fn a_click_elsewhere_does_not_close_the_window() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = resolved();
        settle(&mut v, &lay, None);
        let (px, py) = lay.plots[1].center();
        assert!(!v.click(px, py, &lay));
        assert!(v.is_open(), "clicking a plot closed the window");
    }

    #[test]
    fn a_closing_window_stops_taking_clicks() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = resolved();
        settle(&mut v, &lay, None);
        v.close();
        let (cx, cy) = lay.close.center();
        assert!(!v.click(cx, cy, &lay), "a click landed during the fade-out");
    }

    #[test]
    fn hovering_the_close_control_springs_up_and_back() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = resolved();
        let c = Some(lay.close.center());
        settle(&mut v, &lay, c);
        assert!(v.close_hover.value > 0.9, "hover never arrived");
        settle(&mut v, &lay, Some((5.0, 5.0)));
        assert!(v.close_hover.value < 0.05, "hover never left");
    }

    #[test]
    fn the_layout_never_produces_a_negative_rectangle() {
        for (w, h) in [(640.0, 400.0), (1600.0, 900.0), (3840.0, 2160.0), (400.0, 1200.0)] {
            let lay = ModalLayout::new(w, h);
            for r in lay
                .tiles
                .iter()
                .chain(lay.plots.iter())
                .chain([&lay.panel, &lay.close, &lay.sector_row])
            {
                assert!(r.w > 0.0 && r.h > 0.0, "{w}x{h} produced {r:?}");
            }
        }
    }

    #[test]
    fn plots_are_stacked_without_overlapping() {
        let lay = ModalLayout::new(1600.0, 900.0);
        for i in 1..PLOTS {
            let a = lay.plots[i - 1];
            let b = lay.plots[i];
            assert!(a.y + a.h <= b.y + 1e-3, "plot {i} overlaps the one above it");
            assert_eq!(a.x, b.x, "plots must share an x axis to be comparable");
            assert_eq!(a.w, b.w);
        }
        let last = lay.plots[PLOTS - 1];
        assert!(last.y + last.h <= lay.footer_y, "the plots run into the footer");
    }

    #[test]
    fn a_session_with_no_usable_data_says_so_instead_of_plotting_zeros() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = DebriefView::default();
        v.begin();
        v.resolve(Arc::new(Debrief::compute(&samples()[..8])));
        settle(&mut v, &lay, None);
        let mut dl = DrawList::default();
        build(&mut dl, &v, &lay, "Forza");
        let segments = dl
            .instances
            .iter()
            .filter(|i| i.meta[0] == crate::ui::SHAPE_SEG)
            .count();
        assert!(segments < 10, "{segments} trace segments drawn from 8 samples");
    }

    #[test]
    fn every_character_the_debrief_draws_exists_in_the_atlas() {
        let lay = ModalLayout::new(1600.0, 900.0);
        let mut v = resolved();
        settle(&mut v, &lay, None);
        let mut dl = DrawList::default();
        dl.clear();
        build(&mut dl, &v, &lay, "Assetto Corsa");
        assert_eq!(dl.unrenderable, 0, "{} characters fell back to '?'", dl.unrenderable);

        // The working state and the empty state draw different strings.
        let mut dl = DrawList::default();
        let mut v2 = DebriefView::default();
        v2.begin();
        settle(&mut v2, &lay, None);
        dl.clear();
        build(&mut dl, &v2, &lay, "Forza");
        assert_eq!(dl.unrenderable, 0);

        // And so do the placeholders a sim with no accelerometer produces, plus
        // the warning a too-short session carries.
        let mut bare = samples();
        for x in &mut bare {
            x.present = telemetry_core::sample::FieldMask::SPEED;
        }
        let mut v3 = DebriefView::default();
        v3.begin();
        v3.resolve(Arc::new(Debrief::compute(&bare[..100])));
        settle(&mut v3, &lay, None);
        let mut dl = DrawList::default();
        dl.clear();
        build(&mut dl, &v3, &lay, "Forza");
        assert_eq!(dl.unrenderable, 0, "a placeholder or warning used a missing glyph");
    }

    #[test]
    fn grid_steps_stay_readable_across_track_lengths() {
        for len in [400.0f32, 1200.0, 3600.0, 7000.0, 25000.0] {
            let step = grid_step(len);
            let lines = len / step;
            assert!(lines <= 12.5, "{len} m gave {lines} gridlines");
            assert!(step > 0.0);
        }
    }
}
