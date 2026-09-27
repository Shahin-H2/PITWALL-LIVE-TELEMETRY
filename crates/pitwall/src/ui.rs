//! Layout and draw-list construction.
//!
//! The UI is immediate mode: every frame this module walks the current
//! animated state and emits a flat list of [`Instance`]s. There is no widget
//! tree, no retained scene graph, and no diffing — for a dashboard whose
//! entire content changes 60 times a second, retained-mode bookkeeping would
//! cost more than simply re-emitting ~300 instances.

use crate::anim::{Pulse, Reconstructor, Smoothed, Spring};
use crate::font;
use telemetry_core::sample::{FieldMask, SimId, TelemetrySample};

pub const SHAPE_GLASS: f32 = 0.0;
pub const SHAPE_FILL: f32 = 1.0;
pub const SHAPE_ARC: f32 = 2.0;
pub const SHAPE_GLYPH: f32 = 3.0;
pub const SHAPE_CIRCLE: f32 = 4.0;
pub const SHAPE_RING: f32 = 5.0;

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Instance {
    /// x, y, width, height — pixels, origin top-left.
    pub rect: [f32; 4],
    pub color: [f32; 4],
    /// Shape-dependent. Rounded rect: `[radius, refraction, tint, rim]`.
    /// Arc: `[_, start_turns, end_turns, thickness]`.
    pub params: [f32; 4],
    /// `[shape, glyph, _, _]`
    pub meta: [f32; 4],
}

/// A transform applied to everything pushed while it is set.
///
/// This is what makes the ignition stagger a two-line change at each call site
/// instead of threading an alpha and an offset through every drawing helper:
/// set the reveal, draw the panel normally, clear it.
#[derive(Clone, Copy, Debug)]
struct Reveal {
    alpha: f32,
    offset: (f32, f32),
    scale: f32,
    pivot: (f32, f32),
}

impl Default for Reveal {
    fn default() -> Self {
        Self { alpha: 1.0, offset: (0.0, 0.0), scale: 1.0, pivot: (0.0, 0.0) }
    }
}

#[derive(Default)]
pub struct DrawList {
    pub instances: Vec<Instance>,
    reveal: Reveal,
}

/// Palette. Kept in one place so the whole cluster stays coherent — a
/// dashboard with six unrelated blues reads as unfinished.
pub mod col {
    // Alpha is the panel's overall opacity; the shader takes 55% of it again
    // as the tint strength. Above ~0.7 the tint drowns the blurred backdrop
    // and the panel reads as opaque plastic rather than glass.
    pub const GLASS: [f32; 4] = [0.17, 0.20, 0.28, 0.46];
    pub const GLASS_DEEP: [f32; 4] = [0.09, 0.11, 0.17, 0.62];
    pub const TEXT: [f32; 4] = [0.88, 0.92, 0.97, 1.0];
    pub const TEXT_DIM: [f32; 4] = [0.55, 0.61, 0.70, 1.0];
    pub const ACCENT: [f32; 4] = [0.25, 0.72, 1.00, 1.0];
    pub const THROTTLE: [f32; 4] = [0.30, 0.88, 0.45, 1.0];
    pub const BRAKE: [f32; 4] = [1.00, 0.28, 0.30, 1.0];
    pub const WARN: [f32; 4] = [1.00, 0.72, 0.15, 1.0];
    pub const TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.09];
    pub const OVERSTEER: [f32; 4] = [1.00, 0.45, 0.25, 1.0];
    pub const UNDERSTEER: [f32; 4] = [0.35, 0.62, 1.00, 1.0];
}

impl DrawList {
    pub fn clear(&mut self) {
        self.instances.clear();
        self.reveal = Reveal::default();
    }

    /// Apply `alpha`, a pixel `offset`, and a `scale` about `pivot` to
    /// everything pushed until [`Self::clear_reveal`].
    pub fn set_reveal(&mut self, alpha: f32, offset: (f32, f32), scale: f32, pivot: (f32, f32)) {
        self.reveal = Reveal { alpha: alpha.clamp(0.0, 1.0), offset, scale, pivot };
    }

    pub fn clear_reveal(&mut self) {
        self.reveal = Reveal::default();
    }

    /// The single choke point every primitive goes through.
    fn push(&mut self, mut i: Instance) {
        let rv = self.reveal;
        // Fully transparent elements are dropped here rather than at the
        // rasteriser: during the first frames of ignition most panels have not
        // arrived yet, and this keeps them out of the instance buffer entirely.
        if rv.alpha <= 0.002 {
            return;
        }
        i.color[3] *= rv.alpha;
        if rv.scale != 1.0 {
            i.rect[0] = rv.pivot.0 + (i.rect[0] - rv.pivot.0) * rv.scale;
            i.rect[1] = rv.pivot.1 + (i.rect[1] - rv.pivot.1) * rv.scale;
            i.rect[2] *= rv.scale;
            i.rect[3] *= rv.scale;
        }
        i.rect[0] += rv.offset.0;
        i.rect[1] += rv.offset.1;
        self.instances.push(i);
    }

    /// A glass panel: blurred backdrop, refraction, rim light.
    ///
    /// `hover` is a 0..1 weight the shader uses to widen the rim, deepen the
    /// refraction, and add an interior bloom.
    pub fn glass(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: [f32; 4],
        hover: f32,
    ) {
        self.push(Instance {
            rect: [x, y, w, h],
            color,
            // refraction strength, tint opacity, rim width
            params: [radius, 1.0 + hover * 0.6, color[3] * 0.55, 1.2],
            meta: [SHAPE_GLASS, 0.0, hover, 0.0],
        });
    }

    pub fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: [f32; 4]) {
        self.push(Instance {
            rect: [x, y, w, h],
            color,
            params: [radius, 0.0, 0.0, 0.0],
            meta: [SHAPE_FILL, 0.0, 0.0, 0.0],
        });
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, color: [f32; 4]) {
        self.push(Instance {
            rect: [cx - r, cy - r, r * 2.0, r * 2.0],
            color,
            params: [0.0; 4],
            meta: [SHAPE_CIRCLE, 0.0, 0.0, 0.0],
        });
    }

    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, thickness: f32, color: [f32; 4]) {
        self.push(Instance {
            rect: [cx - r, cy - r, r * 2.0, r * 2.0],
            color,
            params: [0.0, 0.0, 0.0, thickness],
            meta: [SHAPE_RING, 0.0, 0.0, 0.0],
        });
    }

    /// Arc sweeping clockwise from `start` to `end`, both in turns measured
    /// from twelve o'clock. `end` may be less than `start`, meaning the sweep
    /// wraps past twelve — which is the normal case for a tachometer.
    pub fn arc(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        thickness: f32,
        start: f32,
        end: f32,
        color: [f32; 4],
    ) {
        if (end - start).abs() < 1e-5 {
            return;
        }
        self.push(Instance {
            rect: [cx - r, cy - r, r * 2.0, r * 2.0],
            color,
            params: [0.0, start, end, thickness],
            meta: [SHAPE_ARC, 0.0, 0.0, 0.0],
        });
    }

    /// Draw a string. `size` is the cap height; `x, y` is the top-left of the
    /// first glyph's ink. Returns the advance width so callers can chain.
    pub fn text(&mut self, x: f32, y: f32, size: f32, color: [f32; 4], s: &str) -> f32 {
        self.text_outlined(x, y, size, color, 0.0, s)
    }

    /// As [`Self::text`], with a dark contour of `outline` distance units
    /// around the glyph.
    ///
    /// The outline costs nothing: it is a second threshold on the distance
    /// sample the fill already took. Used on the hero numerals, where a light
    /// glyph over bright glass would otherwise lose its edge.
    pub fn text_outlined(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        color: [f32; 4],
        outline: f32,
        s: &str,
    ) -> f32 {
        let m = font::metrics(size);
        let mut cx = x;
        for ch in s.chars() {
            if ch != ' ' {
                self.push(Instance {
                    // The quad is the whole atlas cell, padding included —
                    // drawing only the ink box would clip the glyph exactly
                    // where its antialiasing lives.
                    rect: [cx - m.pad, y - m.pad, m.quad_w, m.quad_h],
                    color,
                    params: [outline, 0.0, 0.0, 0.0],
                    meta: [SHAPE_GLYPH, font::glyph_index(ch), 0.0, 0.0],
                });
            }
            cx += m.advance;
        }
        cx - x
    }

    pub fn text_width(size: f32, s: &str) -> f32 {
        font::text_width(size, s)
    }

    pub fn text_centered(&mut self, cx: f32, y: f32, size: f32, color: [f32; 4], s: &str) {
        let w = Self::text_width(size, s);
        self.text(cx - w * 0.5, y, size, color, s);
    }

    pub fn text_centered_outlined(
        &mut self,
        cx: f32,
        y: f32,
        size: f32,
        color: [f32; 4],
        outline: f32,
        s: &str,
    ) {
        let w = Self::text_width(size, s);
        self.text_outlined(cx - w * 0.5, y, size, color, outline, s);
    }

    pub fn text_right(&mut self, right: f32, y: f32, size: f32, color: [f32; 4], s: &str) {
        let w = Self::text_width(size, s);
        self.text(right - w, y, size, color, s);
    }
}

// ============================================================================
// Layout
// ============================================================================

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
}

/// The interactive panels, in the order they reveal during ignition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum PanelId {
    TopBar = 0,
    Tach = 1,
    Speed = 2,
    Pedals = 3,
    GForce = 4,
    Balance = 5,
    FrameTime = 6,
}

pub const PANEL_COUNT: usize = 7;

impl PanelId {
    pub const ALL: [PanelId; PANEL_COUNT] = [
        PanelId::TopBar,
        PanelId::Tach,
        PanelId::Speed,
        PanelId::Pedals,
        PanelId::GForce,
        PanelId::Balance,
        PanelId::FrameTime,
    ];
    #[inline]
    pub const fn idx(self) -> usize {
        self as usize
    }
    /// Fraction of the ignition sequence at which this panel starts to arrive.
    /// Staggering is what makes a transition read as choreographed rather than
    /// as everything simply fading in at once.
    pub const fn reveal_delay(self) -> f32 {
        match self {
            PanelId::TopBar => 0.00,
            PanelId::Tach => 0.08,
            PanelId::Speed => 0.20,
            PanelId::Pedals => 0.27,
            PanelId::GForce => 0.34,
            PanelId::Balance => 0.41,
            PanelId::FrameTime => 0.48,
        }
    }
}

/// Every panel rectangle, derived purely from the window size.
///
/// Pulled out of the draw code on purpose. Hit-testing for hover needs the
/// same rectangles the renderer uses, and the only way to guarantee they agree
/// is for both to read them from here — a layout computed twice is a layout
/// that drifts.
#[derive(Clone, Debug)]
pub struct Layout {
    /// Global scale factor, so the cluster keeps its proportions from a small
    /// window to a 5K display.
    pub s: f32,
    pub pad: f32,
    pub top_bar: Rect,
    pub tach: Rect,
    pub tach_center: (f32, f32),
    pub tach_r: f32,
    pub speed: Rect,
    pub pedals: Rect,
    pub gforce: Rect,
    pub balance: Rect,
    pub frame_time: Rect,
}

impl Layout {
    pub fn new(w: f32, h: f32) -> Self {
        let s = (h / 900.0).clamp(0.55, 2.4);
        let pad = 22.0 * s;

        let bar_h = 46.0 * s;
        let top_bar = Rect { x: pad, y: pad, w: w - pad * 2.0, h: bar_h };

        let tach_r = (h * 0.26).min(w * 0.22);
        let tach_cx = w * 0.28;
        let tach_cy = h * 0.50;
        let panel = tach_r * 2.16;
        let tach = Rect {
            x: tach_cx - panel * 0.5,
            y: tach_cy - panel * 0.5,
            w: panel,
            h: panel,
        };

        let col_x = w * 0.52;
        let col_w = w * 0.20;
        let speed = Rect { x: col_x, y: h * 0.30, w: col_w, h: h * 0.18 };
        let pedals = Rect {
            x: col_x,
            y: speed.y + speed.h + 18.0 * s,
            w: col_w,
            h: h * 0.30,
        };

        let gg_size = (w * 0.20).min(h * 0.34);
        let gforce = Rect { x: w * 0.755, y: speed.y, w: gg_size, h: gg_size };
        let balance = Rect {
            x: gforce.x,
            y: gforce.y + gg_size + 18.0 * s,
            w: gg_size,
            h: 76.0 * s,
        };

        let ft_h = 54.0 * s;
        let frame_time = Rect { x: pad, y: h - pad - ft_h, w: w * 0.26, h: ft_h };

        Self {
            s,
            pad,
            top_bar,
            tach,
            tach_center: (tach_cx, tach_cy),
            tach_r,
            speed,
            pedals,
            gforce,
            balance,
            frame_time,
        }
    }

    pub fn rect(&self, id: PanelId) -> Rect {
        match id {
            PanelId::TopBar => self.top_bar,
            PanelId::Tach => self.tach,
            PanelId::Speed => self.speed,
            PanelId::Pedals => self.pedals,
            PanelId::GForce => self.gforce,
            PanelId::Balance => self.balance,
            PanelId::FrameTime => self.frame_time,
        }
    }

    /// Which panel is under the cursor, if any.
    ///
    /// The tacho is a circle inside a square rect, so it gets a radial test —
    /// hovering the empty corner of its bounding box should not light it up.
    pub fn hit(&self, px: f32, py: f32) -> Option<PanelId> {
        for id in PanelId::ALL {
            let r = self.rect(id);
            if !r.contains(px, py) {
                continue;
            }
            if id == PanelId::Tach {
                let (cx, cy) = r.center();
                let dx = px - cx;
                let dy = py - cy;
                if (dx * dx + dy * dy).sqrt() > r.w * 0.5 {
                    continue;
                }
            }
            return Some(id);
        }
        None
    }
}

// ============================================================================
// Ignition sequence
// ============================================================================

/// Which phase of the boot sequence the app is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Waiting on the user. Nothing but the wordmark and the prompt.
    Standby,
    /// The transition itself.
    Igniting,
    /// The dashboard, fully live.
    Live,
}

/// Drives the boot sequence.
///
/// One scalar, `t`, running 0 -> 1 over [`IGNITION_S`]. Everything the
/// transition does — the standby screen leaving, the panels arriving, the
/// needle sweep — is a function of that scalar, so the whole sequence stays
/// reproducible and can be scrubbed to any point for a screenshot.
pub struct Ignition {
    pub phase: Phase,
    pub t: f32,
    /// Seconds the app has been in standby, for the idle animations.
    pub standby_t: f32,
}

/// How long the transition takes. Long enough to read as deliberate, short
/// enough that someone who has seen it a hundred times is not held hostage.
pub const IGNITION_S: f32 = 1.9;

impl Default for Ignition {
    fn default() -> Self {
        Self { phase: Phase::Standby, t: 0.0, standby_t: 0.0 }
    }
}

impl Ignition {
    /// Begin the transition. Ignored if it is already running or finished.
    pub fn start(&mut self) -> bool {
        if self.phase == Phase::Standby {
            self.phase = Phase::Igniting;
            self.t = 0.0;
            true
        } else {
            false
        }
    }

    pub fn update(&mut self, dt: f32) {
        match self.phase {
            Phase::Standby => self.standby_t += dt,
            Phase::Igniting => {
                self.t += dt / IGNITION_S;
                if self.t >= 1.0 {
                    self.t = 1.0;
                    self.phase = Phase::Live;
                }
            }
            Phase::Live => {}
        }
    }

    /// Progress of one staggered stage, eased.
    pub fn stage(&self, delay: f32, dur: f32) -> f32 {
        if self.phase == Phase::Live {
            return 1.0;
        }
        if self.phase == Phase::Standby {
            return 0.0;
        }
        ease_out_cubic(((self.t - delay) / dur.max(1e-3)).clamp(0.0, 1.0))
    }

    /// How present the standby screen still is, 1 -> 0 as ignition begins.
    pub fn standby_alpha(&self) -> f32 {
        match self.phase {
            Phase::Standby => 1.0,
            Phase::Live => 0.0,
            // Leaves quickly, so it is gone before the dashboard is legible
            // and the two never fight for attention.
            Phase::Igniting => 1.0 - ease_out_cubic((self.t / 0.28).clamp(0.0, 1.0)),
        }
    }

    /// The ignition needle sweep, as a multiplier on the live RPM fraction.
    ///
    /// A real car sweeps its tachometer to the stop and back when you turn the
    /// key — a self-test that became a piece of theatre. Reproducing it is the
    /// single cue that makes this read as a machine starting up rather than a
    /// web page loading.
    ///
    /// Returns `None` once the sweep is done and the needle should follow the
    /// live value.
    pub fn needle_sweep(&self) -> Option<f32> {
        if self.phase != Phase::Igniting {
            return None;
        }
        // Out over the first 45% of the sequence, back over the next 30%.
        const OUT_END: f32 = 0.45;
        const BACK_END: f32 = 0.75;
        if self.t < OUT_END {
            Some(ease_out_cubic(self.t / OUT_END))
        } else if self.t < BACK_END {
            let k = (self.t - OUT_END) / (BACK_END - OUT_END);
            Some(1.0 - ease_in_out_cubic(k))
        } else {
            None
        }
    }
}

#[inline]
pub fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

#[inline]
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let u = -2.0 * t + 2.0;
        1.0 - u * u * u * 0.5
    }
}

// ============================================================================
// Animated state
// ============================================================================

/// Everything on screen that moves, with its own animation model.
///
/// Note the deliberately different response times. The tachometer must not lag
/// the engine, so it is stiff; the pedal bars are near-instant because a
/// driver reads them as a direct echo of their own feet; the g-g dot is
/// heavier because it represents a physical mass.
pub struct Dash {
    pub rpm: Spring,
    pub throttle: Spring,
    pub brake: Spring,
    pub speed: Smoothed,
    pub gear_display: i8,
    pub gear_flash: f32,
    pub lat_g: Spring,
    pub long_g: Spring,
    pub balance: Smoothed,
    pub connected: Smoothed,
    pub limiter: Smoothed,
    pub pulse: Pulse,

    /// Per-panel hover, 0 -> 1.
    ///
    /// Springs rather than a boolean, because the whole point of a hover state
    /// is the *motion* into and out of it. Slightly under-damped so the panel
    /// settles with a hint of overshoot — that tiny elasticity is what makes a
    /// surface feel physical instead of switched.
    pub hover: [Spring; PANEL_COUNT],

    // Reconstructors let us render at 240 Hz off a 60 Hz feed.
    pub rpm_recon: Reconstructor,
    pub speed_recon: Reconstructor,

    pub frame_history: Vec<f32>,
    pub last_gear: i8,
}

impl Default for Dash {
    fn default() -> Self {
        Self::new()
    }
}

impl Dash {
    pub fn new() -> Self {
        Self {
            rpm: Spring::new(0.0, 0.07),
            throttle: Spring::new(0.0, 0.05),
            brake: Spring::new(0.0, 0.05),
            speed: Smoothed::new(0.0, 0.08),
            gear_display: 0,
            gear_flash: 0.0,
            lat_g: Spring::new(0.0, 0.12),
            long_g: Spring::new(0.0, 0.12),
            balance: Smoothed::new(0.0, 0.25),
            connected: Smoothed::new(0.0, 0.4),
            limiter: Smoothed::new(0.0, 0.06),
            pulse: Pulse::default(),
            hover: std::array::from_fn(|_| Spring::new(0.0, 0.22).with_damping(0.72)),
            rpm_recon: Reconstructor::new(0.018),
            speed_recon: Reconstructor::new(0.018),
            frame_history: Vec::with_capacity(240),
            last_gear: 0,
        }
    }

    /// Feed a newly arrived telemetry sample into the reconstructors.
    pub fn on_sample(&mut self, s: &TelemetrySample, t: f64) {
        self.rpm_recon.push(s.rpm_fraction(), t);
        self.speed_recon.push(s.speed_kmh(), t);
    }

    /// Jump every animated value straight to the sample, skipping the
    /// transition.
    ///
    /// Called when telemetry first arrives, or resumes after a gap. Without
    /// this, firing up the app with the car already at 8000 rpm produces a
    /// second-long sweep from zero that looks like the engine revving — which
    /// is not just ugly, it is telemetry the driver did not generate.
    pub fn snap_to(&mut self, s: &TelemetrySample) {
        // Seed the reconstructors too. `update` always reads the needle target
        // from them, so a dash that was snapped but never fed a sample would
        // spring straight back to zero on the very next frame.
        self.rpm_recon.push(s.rpm_fraction(), 0.0);
        self.rpm_recon.push(s.rpm_fraction(), 1.0);
        self.speed_recon.push(s.speed_kmh(), 0.0);
        self.speed_recon.push(s.speed_kmh(), 1.0);
        self.rpm.snap(s.rpm_fraction());
        self.throttle.snap(s.throttle);
        self.brake.snap(s.brake);
        self.speed.snap(s.speed_kmh());
        self.lat_g.snap(s.lat_g());
        self.long_g.snap(s.long_g());
        self.balance.snap(s.balance().unwrap_or(0.0));
        self.last_gear = s.gear;
        self.gear_display = s.gear;
    }

    /// Advance all animation by `dt`, toward the latest sample.
    ///
    /// `sweep` overrides the RPM needle during the ignition self-test.
    pub fn update(
        &mut self,
        s: &TelemetrySample,
        now: f64,
        dt: f32,
        connected: bool,
        hovered: Option<PanelId>,
        sweep: Option<f32>,
    ) {
        let rpm_target = sweep.unwrap_or_else(|| self.rpm_recon.sample(now));
        self.rpm.update(rpm_target, dt);
        self.throttle.update(s.throttle, dt);
        self.brake.update(s.brake, dt);
        self.speed.update(self.speed_recon.sample(now), dt);
        self.lat_g.update(s.lat_g(), dt);
        self.long_g.update(s.long_g(), dt);
        self.balance.update(s.balance().unwrap_or(0.0), dt);
        self.connected.update(if connected { 1.0 } else { 0.0 }, dt);

        for id in PanelId::ALL {
            let target = if hovered == Some(id) { 1.0 } else { 0.0 };
            self.hover[id.idx()].update(target, dt);
        }

        // The limiter light is a threshold on the *raw* value, not the
        // smoothed needle — it must fire the instant the engine is on the
        // limit, not once the needle has caught up.
        let on_limiter = s.rpm_fraction() > 0.985;
        self.limiter.update(if on_limiter { 1.0 } else { 0.0 }, dt);
        self.pulse.update(9.0, dt);

        if s.gear != self.last_gear {
            self.gear_flash = 1.0;
            self.last_gear = s.gear;
        }
        self.gear_display = s.gear;
        self.gear_flash = (self.gear_flash - dt * 4.0).max(0.0);

        self.frame_history.push(dt * 1000.0);
        if self.frame_history.len() > 240 {
            self.frame_history.remove(0);
        }
    }

    pub fn shift_pulse(&mut self) -> f32 {
        self.pulse.update(9.0, 0.0)
    }

    #[inline]
    pub fn hover_of(&self, id: PanelId) -> f32 {
        self.hover[id.idx()].value.clamp(0.0, 1.4)
    }
}

fn gear_label(g: i8) -> String {
    match g {
        -1 => "R".into(),
        0 => "N".into(),
        n => n.to_string(),
    }
}

// ============================================================================
// The standby screen
// ============================================================================

/// The pre-ignition screen: wordmark, prompt, and nothing else.
///
/// Restraint is the point. This screen exists to be looked at for two seconds
/// and then never again, so it carries one idea — the machine is armed and
/// waiting — and no data. Everything on it is procedural, so it costs nothing
/// to ship and scales to any resolution.
pub fn build_standby(dl: &mut DrawList, ign: &Ignition, w: f32, h: f32, sim: SimId, connected: bool) {
    let alpha = ign.standby_alpha();
    if alpha <= 0.001 {
        return;
    }
    let s = (h / 900.0).clamp(0.55, 2.4);
    let t = ign.standby_t;
    let cx = w * 0.5;
    let cy = h * 0.46;

    // As ignition starts, the whole standby group lifts and scales away
    // slightly — it recedes rather than merely fading, which reads as the
    // screen handing over rather than dissolving.
    let exit = 1.0 - alpha;
    dl.set_reveal(alpha, (0.0, -exit * 40.0 * s), 1.0 + exit * 0.06, (cx, cy));

    // A slow breathing ring behind the wordmark: the only thing moving, so it
    // is what tells you the app is alive and not hung.
    let ring_r = 150.0 * s;
    let breathe = (t * 0.9).sin() * 0.5 + 0.5;
    dl.ring(cx, cy, ring_r, 1.2 * s, [0.30, 0.42, 0.58, 0.20 + breathe * 0.14]);
    dl.ring(cx, cy, ring_r * 0.78, 1.0 * s, [0.30, 0.42, 0.58, 0.10]);

    // A single indicator orbiting the ring, like a scanning sweep.
    let orbit = t * 0.55;
    let (osin, ocos) = (orbit.sin(), orbit.cos());
    dl.circle(cx + ocos * ring_r, cy + osin * ring_r, 4.0 * s, col::ACCENT);
    dl.circle(
        cx + ocos * ring_r,
        cy + osin * ring_r,
        11.0 * s,
        [col::ACCENT[0], col::ACCENT[1], col::ACCENT[2], 0.22],
    );

    // Wordmark, with the subtitle set clear of its baseline and flanked by
    // rules — the rules are what stop a lone small caption looking stranded.
    dl.text_centered(cx, cy - 42.0 * s, 58.0 * s, col::TEXT, "PITWALL");
    let sub_w = DrawList::text_width(13.0 * s, "LIVE TELEMETRY");
    let rule_y = cy + 48.0 * s;
    dl.fill(cx - sub_w * 0.5 - 30.0 * s, rule_y, 22.0 * s, 1.0 * s, 0.0, col::TEXT_DIM);
    dl.fill(cx + sub_w * 0.5 + 8.0 * s, rule_y, 22.0 * s, 1.0 * s, 0.0, col::TEXT_DIM);
    dl.text_centered(cx, cy + 42.0 * s, 13.0 * s, col::TEXT_DIM, "LIVE TELEMETRY");

    // The prompt. Pulsed, because a static line reads as a label and a pulsing
    // one reads as an invitation.
    let pulse = 0.55 + 0.45 * ((t * 2.1).sin() * 0.5 + 0.5);
    dl.text_centered(
        cx,
        h * 0.76,
        17.0 * s,
        [col::TEXT[0], col::TEXT[1], col::TEXT[2], pulse],
        "PRESS ANY KEY TO INITIALIZE",
    );

    // Link status, so the driver knows whether the sim is already talking
    // before they commit.
    let (dot, label) = if connected {
        (col::THROTTLE, format!("{} LINKED", sim.name().to_uppercase()))
    } else {
        (col::WARN, "AWAITING TELEMETRY".to_string())
    };
    let lw = DrawList::text_width(11.0 * s, &label);
    dl.circle(cx - lw * 0.5 - 14.0 * s, h * 0.83 + 5.0 * s, 3.5 * s, dot);
    dl.text_centered(cx, h * 0.83, 11.0 * s, col::TEXT_DIM, &label);

    dl.clear_reveal();
}

// ============================================================================
// The dashboard
// ============================================================================

/// Build a full frame's draw list.
#[allow(clippy::too_many_arguments)]
pub fn build(
    dl: &mut DrawList,
    dash: &Dash,
    lay: &Layout,
    ign: &Ignition,
    sample: &TelemetrySample,
    sim: SimId,
    w: f32,
    h: f32,
    pkt_rate: f32,
    pinned: bool,
) {
    if ign.phase == Phase::Standby {
        return;
    }
    let s = lay.s;
    let pad = lay.pad;

    // Each panel arrives on its own schedule, rising into place.
    let enter = |dl: &mut DrawList, id: PanelId, r: Rect| -> f32 {
        let k = ign.stage(id.reveal_delay(), 0.42);
        let (cx, cy) = r.center();
        dl.set_reveal(k, (0.0, (1.0 - k) * 34.0 * s), 0.94 + 0.06 * k, (cx, cy));
        k
    };

    // ================= top status bar =================
    let r = lay.top_bar;
    enter(dl, PanelId::TopBar, r);
    glass_panel(dl, r, 14.0 * s, col::GLASS_DEEP, dash.hover_of(PanelId::TopBar));

    let connected = dash.connected.value() > 0.5;
    let dot_col = if connected { col::THROTTLE } else { col::BRAKE };
    dl.circle(r.x + 22.0 * s, r.y + r.h * 0.5, 5.0 * s, dot_col);

    let label = if sim == SimId::Unknown {
        "WAITING FOR TELEMETRY".to_string()
    } else {
        sim.name().to_uppercase()
    };
    dl.text(r.x + 40.0 * s, r.y + r.h * 0.5 - 7.0 * s, 14.0 * s, col::TEXT, &label);

    if pinned {
        dl.text(
            r.x + 40.0 * s + DrawList::text_width(14.0 * s, &label) + 14.0 * s,
            r.y + r.h * 0.5 - 6.0 * s,
            11.0 * s,
            col::WARN,
            "PINNED",
        );
    }
    dl.text_right(
        r.x + r.w - 20.0 * s,
        r.y + r.h * 0.5 - 6.0 * s,
        12.0 * s,
        col::TEXT_DIM,
        &format!("{:.0} PKT/S", pkt_rate),
    );

    // ================= tachometer =================
    let r = lay.tach;
    enter(dl, PanelId::Tach, r);
    let (tach_cx, tach_cy) = lay.tach_center;
    let tach_r = lay.tach_r;
    let hov = dash.hover_of(PanelId::Tach);
    glass_panel_round(dl, r, col::GLASS, hov);

    // The dial runs from 7:30 clockwise over the top to 4:30 — 0.75 turns of
    // travel, wrapping past twelve. That is the convention every real cluster
    // uses, and it keeps the bottom of the dial clear for the gear numeral.
    let a0 = 0.625_f32;
    let sweep_turns = 0.75_f32;
    let a1 = (a0 + sweep_turns) % 1.0;

    dl.arc(tach_cx, tach_cy, tach_r, 12.0 * s, a0, a1, col::TRACK);

    let rpm_f = dash.rpm.value.clamp(0.0, 1.0);
    let sweep_end = (a0 + sweep_turns * rpm_f) % 1.0;

    // Colour the needle band by how close to the limiter we are — green
    // through amber to red. The driver reads colour before they read position.
    let bar_col = if rpm_f > 0.95 {
        col::BRAKE
    } else if rpm_f > 0.82 {
        col::WARN
    } else {
        col::ACCENT
    };
    if rpm_f > 0.001 {
        dl.arc(tach_cx, tach_cy, tach_r, 12.0 * s, a0, sweep_end, bar_col);
    }

    let redline_start = (a0 + sweep_turns * 0.93) % 1.0;
    dl.arc(tach_cx, tach_cy, tach_r + 11.0 * s, 3.0 * s, redline_start, a1, col::BRAKE);

    for i in 0..=10 {
        let f = i as f32 / 10.0;
        let a = (a0 + sweep_turns * f) % 1.0;
        let ang = (a - 0.25) * std::f32::consts::TAU;
        let (sn, cs) = ang.sin_cos();
        let r_in = tach_r - 20.0 * s;
        let tick_len = if i % 5 == 0 { 10.0 * s } else { 5.0 * s };
        let tx = tach_cx + cs * (r_in - tick_len * 0.5);
        let ty = tach_cy + sn * (r_in - tick_len * 0.5);
        dl.circle(tx, ty, 1.6 * s, col::TEXT_DIM);
    }

    // Gear, dead centre — the single largest element on screen, because it is
    // the one thing a driver glances at mid-corner.
    let gear_size = tach_r * 0.62;
    let flash = dash.gear_flash;
    let gear_col = [
        col::TEXT[0] + flash * 0.4,
        col::TEXT[1] + flash * 0.3,
        col::TEXT[2],
        1.0,
    ];
    dl.text_centered_outlined(
        tach_cx,
        tach_cy - gear_size * 0.62,
        gear_size,
        gear_col,
        0.055,
        &gear_label(dash.gear_display),
    );
    dl.text_centered(
        tach_cx,
        tach_cy + gear_size * 0.52,
        16.0 * s,
        col::TEXT_DIM,
        &format!("{:.0} RPM", sample.rpm),
    );

    // ================= speed =================
    let r = lay.speed;
    enter(dl, PanelId::Speed, r);
    glass_panel(dl, r, 18.0 * s, col::GLASS, dash.hover_of(PanelId::Speed));
    dl.text_centered(
        r.x + r.w * 0.5,
        r.y + r.h * 0.20,
        r.h * 0.46,
        col::TEXT,
        &format!("{:.0}", dash.speed.value()),
    );
    dl.text_centered(r.x + r.w * 0.5, r.y + r.h * 0.78, 13.0 * s, col::TEXT_DIM, "KM/H");

    // ================= pedals =================
    let r = lay.pedals;
    enter(dl, PanelId::Pedals, r);
    glass_panel(dl, r, 18.0 * s, col::GLASS, dash.hover_of(PanelId::Pedals));

    let inner_pad = 20.0 * s;
    let bar_w = (r.w - inner_pad * 3.0) * 0.5;
    let bar_top = r.y + inner_pad + 16.0 * s;
    let bar_h = r.h - inner_pad * 2.0 - 30.0 * s;

    let pedal_bar = |dl: &mut DrawList, bx: f32, v: f32, c: [f32; 4], label: &str| {
        dl.fill(bx, bar_top, bar_w, bar_h, 6.0 * s, col::TRACK);
        let fh = (bar_h * v.clamp(0.0, 1.0)).max(1.0);
        dl.fill(bx, bar_top + bar_h - fh, bar_w, fh, 6.0 * s, c);
        dl.text_centered(bx + bar_w * 0.5, r.y + inner_pad * 0.35, 12.0 * s, col::TEXT_DIM, label);
        dl.text_centered(
            bx + bar_w * 0.5,
            bar_top + bar_h + 8.0 * s,
            12.0 * s,
            c,
            &format!("{:.0}", v * 100.0),
        );
    };
    pedal_bar(dl, r.x + inner_pad, dash.throttle.value, col::THROTTLE, "THR");
    pedal_bar(dl, r.x + inner_pad * 2.0 + bar_w, dash.brake.value, col::BRAKE, "BRK");

    // ================= g-g diagram =================
    let r = lay.gforce;
    enter(dl, PanelId::GForce, r);
    glass_panel(dl, r, 18.0 * s, col::GLASS, dash.hover_of(PanelId::GForce));

    let (gcx, gcy) = r.center();
    let g_r = r.w * 0.34;
    dl.ring(gcx, gcy, g_r * 0.5, 1.0 * s, col::TRACK);
    dl.ring(gcx, gcy, g_r, 1.5 * s, col::TRACK);
    dl.fill(gcx - g_r, gcy - 0.5 * s, g_r * 2.0, 1.0 * s, 0.0, col::TRACK);
    dl.fill(gcx - 0.5 * s, gcy - g_r, 1.0 * s, g_r * 2.0, 0.0, col::TRACK);

    let gx = gcx + (dash.lat_g.value / 2.0).clamp(-1.2, 1.2) * g_r;
    let gy = gcy - (dash.long_g.value / 2.0).clamp(-1.2, 1.2) * g_r;
    dl.circle(gx, gy, 11.0 * s, [col::ACCENT[0], col::ACCENT[1], col::ACCENT[2], 0.25]);
    dl.circle(gx, gy, 6.0 * s, col::ACCENT);

    dl.text_centered(gcx, r.y + 12.0 * s, 12.0 * s, col::TEXT_DIM, "G-G");
    dl.text_centered(
        gcx,
        r.y + r.h - 24.0 * s,
        12.0 * s,
        col::TEXT_DIM,
        &format!("{:.2}G", sample.combined_g()),
    );

    // ================= balance =================
    let r = lay.balance;
    enter(dl, PanelId::Balance, r);
    glass_panel(dl, r, 16.0 * s, col::GLASS, dash.hover_of(PanelId::Balance));
    dl.text_centered(r.x + r.w * 0.5, r.y + 10.0 * s, 11.0 * s, col::TEXT_DIM, "BALANCE");

    let track_y = r.y + r.h * 0.58;
    let track_w = r.w - 40.0 * s;
    let track_x = r.x + 20.0 * s;
    dl.fill(track_x, track_y - 2.0 * s, track_w, 4.0 * s, 2.0 * s, col::TRACK);
    dl.fill(track_x + track_w * 0.5 - 1.0 * s, track_y - 8.0 * s, 2.0 * s, 16.0 * s, 0.0, col::TEXT_DIM);

    if sample.has(FieldMask::SLIP_ANGLE) {
        // Scale: ±0.10 rad (~5.7°) spans the full width. Beyond that the car
        // is not "slightly loose", it is sideways.
        let b = (dash.balance.value() / 0.10).clamp(-1.0, 1.0);
        let bx = track_x + track_w * 0.5 + b * track_w * 0.5;
        let bc = if b > 0.0 { col::OVERSTEER } else { col::UNDERSTEER };
        dl.circle(bx, track_y, 7.0 * s, bc);
        let word = if b > 0.12 {
            "OVERSTEER"
        } else if b < -0.12 {
            "UNDERSTEER"
        } else {
            "NEUTRAL"
        };
        dl.text_centered(r.x + r.w * 0.5, r.y + r.h - 20.0 * s, 11.0 * s, bc, word);
    } else {
        dl.text_centered(r.x + r.w * 0.5, r.y + r.h - 20.0 * s, 10.0 * s, col::TEXT_DIM, "NO SLIP DATA");
    }

    // ================= frame-time histogram =================
    // Permanently on screen, not behind a debug flag. The entire architecture
    // is a bet on frame consistency; if that bet is ever lost, it should be
    // visible immediately rather than discovered in a bug report.
    let r = lay.frame_time;
    enter(dl, PanelId::FrameTime, r);
    glass_panel(dl, r, 12.0 * s, col::GLASS_DEEP, dash.hover_of(PanelId::FrameTime));
    dl.text(r.x + 12.0 * s, r.y + 8.0 * s, 10.0 * s, col::TEXT_DIM, "FRAME MS");

    let hist = &dash.frame_history;
    if !hist.is_empty() {
        let plot_x = r.x + 12.0 * s;
        let plot_y = r.y + 22.0 * s;
        let plot_w = r.w - 24.0 * s;
        let plot_h = r.h - 30.0 * s;
        let bw = (plot_w / hist.len() as f32).max(0.6);

        // Colour relative to the *observed* frame budget, not a fixed number.
        // A 16.7 ms frame is perfect on a 60 Hz panel and a dropped frame on a
        // 240 Hz one; a hardcoded threshold calls the first of those a warning
        // and paints a healthy dashboard permanently amber.
        let mut sorted: Vec<f32> = hist.clone();
        sorted.sort_by(f32::total_cmp);
        let budget = sorted[sorted.len() / 2].max(0.1);
        let scale = (budget * 3.0).max(4.0);

        for (i, ms) in hist.iter().enumerate() {
            let f = (ms / scale).clamp(0.0, 1.0);
            let bh = (plot_h * f).max(0.5);
            let c = if *ms > budget * 1.9 {
                col::BRAKE
            } else if *ms > budget * 1.35 {
                col::WARN
            } else {
                col::THROTTLE
            };
            dl.fill(plot_x + i as f32 * bw, plot_y + plot_h - bh, bw * 0.85, bh, 0.0, c);
        }
        let avg: f32 = hist.iter().sum::<f32>() / hist.len() as f32;
        let worst = sorted[sorted.len() - 1];
        dl.text_right(
            r.x + r.w - 12.0 * s,
            r.y + 8.0 * s,
            10.0 * s,
            col::TEXT_DIM,
            &format!("{avg:.1} AVG  {worst:.1} PEAK"),
        );
    }

    // ================= hint bar =================
    let k = ign.stage(0.55, 0.4);
    dl.set_reveal(k, (0.0, 0.0), 1.0, (0.0, 0.0));
    dl.text(
        w * 0.52,
        h - pad - 16.0 * s,
        11.0 * s,
        col::TEXT_DIM,
        "TAB SWITCH SIM   P PIN   R RECORD   ESC QUIT",
    );
    dl.clear_reveal();
}

/// A glass panel whose rim and refraction respond to hover.
fn glass_panel(dl: &mut DrawList, r: Rect, radius: f32, base: [f32; 4], hover: f32) {
    // Hover brightens the tint, widens the rim, and grows the panel by a
    // fraction of a percent. All three are already shader or geometry
    // parameters, so an interactive state costs no extra draw call and no
    // extra pass — the panel simply lights up and leans forward.
    //
    // The growth is deliberately tiny (~0.7% at full hover). Large enough to
    // register as the surface coming toward you, small enough that it never
    // reads as the layout shifting under the cursor.
    let lift = hover * 0.14;
    let color = [
        base[0] + lift * 0.55,
        base[1] + lift * 0.60,
        base[2] + lift * 0.75,
        base[3],
    ];
    let grow = hover * 0.007;
    let dw = r.w * grow;
    let dh = r.h * grow;
    dl.glass(
        r.x - dw * 0.5,
        r.y - dh * 0.5,
        r.w + dw,
        r.h + dh,
        radius,
        color,
        hover,
    );
}

fn glass_panel_round(dl: &mut DrawList, r: Rect, base: [f32; 4], hover: f32) {
    glass_panel(dl, r, r.w * 0.5, base, hover);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dash_with_sample() -> (Dash, TelemetrySample) {
        let s = TelemetrySample {
            present: FieldMask::SLIP_ANGLE | FieldMask::GEAR | FieldMask::RPM,
            rpm: 6000.0,
            max_rpm: 8000.0,
            gear: 4,
            ..Default::default()
        };
        let mut d = Dash::new();
        d.snap_to(&s);
        (d, s)
    }

    fn live() -> Ignition {
        Ignition { phase: Phase::Live, t: 1.0, standby_t: 0.0 }
    }

    // ---- layout -----------------------------------------------------------

    #[test]
    fn panels_do_not_overlap() {
        // Overlapping rects would make hover ambiguous and hit-testing
        // order-dependent.
        let l = Layout::new(1600.0, 900.0);
        let rects: Vec<_> = PanelId::ALL.iter().map(|id| (*id, l.rect(*id))).collect();
        for (i, (ida, a)) in rects.iter().enumerate() {
            for (idb, b) in rects.iter().skip(i + 1) {
                let overlap = a.x < b.x + b.w
                    && b.x < a.x + a.w
                    && a.y < b.y + b.h
                    && b.y < a.y + a.h;
                assert!(!overlap, "{ida:?} overlaps {idb:?}");
            }
        }
    }

    #[test]
    fn hit_testing_finds_each_panel_at_its_centre() {
        let l = Layout::new(1600.0, 900.0);
        for id in PanelId::ALL {
            let (cx, cy) = l.rect(id).center();
            assert_eq!(l.hit(cx, cy), Some(id), "{id:?} not hit at its centre");
        }
    }

    #[test]
    fn the_tacho_hit_test_is_radial_not_rectangular() {
        // The tacho is a circle in a square rect. Its bounding-box corner is
        // empty screen and must not light the panel up.
        let l = Layout::new(1600.0, 900.0);
        let r = l.rect(PanelId::Tach);
        assert_eq!(l.hit(r.x + 2.0, r.y + 2.0), None, "corner of the bbox should miss");
        let (cx, cy) = r.center();
        assert_eq!(l.hit(cx, cy), Some(PanelId::Tach));
    }

    #[test]
    fn empty_space_hits_nothing() {
        let l = Layout::new(1600.0, 900.0);
        assert_eq!(l.hit(-50.0, -50.0), None);
        assert_eq!(l.hit(1599.0, 899.0), None);
    }

    // ---- ignition ---------------------------------------------------------

    #[test]
    fn ignition_runs_standby_to_live_exactly_once() {
        let mut ign = Ignition::default();
        assert_eq!(ign.phase, Phase::Standby);
        assert!(ign.start(), "first start should take");
        assert!(!ign.start(), "a second start must not restart the sequence");
        assert_eq!(ign.phase, Phase::Igniting);

        for _ in 0..(60.0 * IGNITION_S) as u32 + 10 {
            ign.update(1.0 / 60.0);
        }
        assert_eq!(ign.phase, Phase::Live);
        assert_eq!(ign.t, 1.0);
        assert!(!ign.start(), "cannot re-ignite from Live");
    }

    #[test]
    fn stages_are_ordered_and_monotonic() {
        let mut ign = Ignition::default();
        ign.start();
        let mut prev = vec![0.0f32; PANEL_COUNT];
        for step in 0..80 {
            ign.update(IGNITION_S / 80.0);
            for id in PanelId::ALL {
                let v = ign.stage(id.reveal_delay(), 0.42);
                assert!(v >= prev[id.idx()] - 1e-5, "{id:?} went backwards at step {step}");
                assert!((0.0..=1.0).contains(&v));
                prev[id.idx()] = v;
            }
        }
        for id in PanelId::ALL {
            assert!((prev[id.idx()] - 1.0).abs() < 1e-4, "{id:?} never finished");
        }
    }

    #[test]
    fn earlier_panels_lead_later_ones() {
        let mut ign = Ignition::default();
        ign.start();
        ign.update(IGNITION_S * 0.3);
        let top = ign.stage(PanelId::TopBar.reveal_delay(), 0.42);
        let last = ign.stage(PanelId::FrameTime.reveal_delay(), 0.42);
        assert!(top > last, "the stagger is inverted: {top} vs {last}");
    }

    #[test]
    fn the_needle_sweeps_out_and_back_then_releases() {
        let mut ign = Ignition::default();
        assert_eq!(ign.needle_sweep(), None, "no sweep while in standby");
        ign.start();

        let mut peak = 0.0f32;
        let mut released_at = None;
        for i in 0..200 {
            ign.update(IGNITION_S / 200.0);
            match ign.needle_sweep() {
                Some(v) => {
                    assert!((0.0..=1.0).contains(&v), "sweep out of range: {v}");
                    peak = peak.max(v);
                }
                None if released_at.is_none() && ign.phase == Phase::Igniting => {
                    released_at = Some(i);
                }
                _ => {}
            }
        }
        assert!(peak > 0.97, "the needle should reach the stop, peaked at {peak}");
        assert!(released_at.is_some(), "the needle must hand back to live data");
        assert_eq!(ign.needle_sweep(), None, "no sweep once Live");
    }

    #[test]
    fn standby_alpha_falls_before_the_dashboard_is_legible() {
        let mut ign = Ignition::default();
        assert_eq!(ign.standby_alpha(), 1.0);
        ign.start();
        ign.update(IGNITION_S * 0.30);
        assert!(ign.standby_alpha() < 0.01, "standby should be gone by 30%");
        assert!(
            ign.stage(PanelId::Tach.reveal_delay(), 0.42) > 0.3,
            "the dashboard should be arriving by then"
        );
    }

    // ---- reveal transform -------------------------------------------------

    #[test]
    fn reveal_scales_alpha_and_offsets_geometry() {
        let mut dl = DrawList::default();
        dl.set_reveal(0.5, (10.0, -20.0), 1.0, (0.0, 0.0));
        dl.fill(100.0, 200.0, 50.0, 60.0, 0.0, [1.0, 1.0, 1.0, 1.0]);
        let i = &dl.instances[0];
        assert_eq!(i.rect[0], 110.0);
        assert_eq!(i.rect[1], 180.0);
        assert!((i.color[3] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn reveal_scales_about_the_pivot() {
        let mut dl = DrawList::default();
        dl.set_reveal(1.0, (0.0, 0.0), 0.5, (100.0, 100.0));
        dl.fill(100.0, 100.0, 40.0, 40.0, 0.0, [1.0; 4]);
        let i = &dl.instances[0];
        assert_eq!((i.rect[0], i.rect[1]), (100.0, 100.0), "the pivot stays put");
        assert_eq!((i.rect[2], i.rect[3]), (20.0, 20.0));
    }

    #[test]
    fn fully_transparent_elements_never_reach_the_buffer() {
        let mut dl = DrawList::default();
        dl.set_reveal(0.0, (0.0, 0.0), 1.0, (0.0, 0.0));
        for _ in 0..50 {
            dl.fill(0.0, 0.0, 10.0, 10.0, 0.0, [1.0; 4]);
        }
        assert!(dl.instances.is_empty(), "invisible instances were still uploaded");
    }

    #[test]
    fn clear_resets_the_transform() {
        let mut dl = DrawList::default();
        dl.set_reveal(0.25, (5.0, 5.0), 2.0, (0.0, 0.0));
        dl.clear();
        dl.fill(10.0, 10.0, 10.0, 10.0, 0.0, [1.0; 4]);
        let i = &dl.instances[0];
        assert_eq!((i.rect[0], i.rect[1]), (10.0, 10.0));
        assert!((i.color[3] - 1.0).abs() < 1e-6);
    }

    // ---- composition ------------------------------------------------------

    #[test]
    fn standby_draws_only_before_the_dashboard() {
        let mut dl = DrawList::default();
        build_standby(&mut dl, &live(), 1600.0, 900.0, SimId::Forza, true);
        assert!(dl.instances.is_empty(), "standby must be gone once Live");

        dl.clear();
        build_standby(&mut dl, &Ignition::default(), 1600.0, 900.0, SimId::Forza, true);
        assert!(dl.instances.len() > 10, "standby should draw in Standby");
    }

    #[test]
    fn the_dashboard_draws_nothing_during_standby() {
        let (dash, s) = dash_with_sample();
        let l = Layout::new(1600.0, 900.0);
        let mut dl = DrawList::default();
        build(&mut dl, &dash, &l, &Ignition::default(), &s, SimId::Forza, 1600.0, 900.0, 60.0, false);
        assert!(dl.instances.is_empty());
    }

    #[test]
    fn a_live_frame_is_populated_and_finite() {
        let (dash, s) = dash_with_sample();
        let l = Layout::new(1600.0, 900.0);
        let mut dl = DrawList::default();
        build(&mut dl, &dash, &l, &live(), &s, SimId::F1_25, 1600.0, 900.0, 60.0, true);
        assert!(dl.instances.len() > 60, "got {}", dl.instances.len());
        for i in &dl.instances {
            assert!((0.0..=5.0).contains(&i.meta[0]), "bad shape tag {}", i.meta[0]);
            assert!(i.rect.iter().all(|v| v.is_finite()), "non-finite rect {:?}", i.rect);
            assert!(i.rect[2] >= 0.0 && i.rect[3] >= 0.0, "negative size {:?}", i.rect);
            assert!(i.color.iter().all(|v| v.is_finite()));
        }
    }

    #[test]
    fn layout_survives_absurd_window_sizes() {
        let (dash, s) = dash_with_sample();
        for (w, h) in [(320.0, 240.0), (7680.0, 4320.0), (100.0, 3000.0), (3000.0, 100.0)] {
            let l = Layout::new(w, h);
            let mut dl = DrawList::default();
            build(&mut dl, &dash, &l, &live(), &s, SimId::Forza, w, h, 60.0, false);
            for i in &dl.instances {
                assert!(i.rect.iter().all(|v| v.is_finite()), "{w}x{h} produced NaN");
            }
        }
    }

    #[test]
    fn hover_reaches_the_instance_stream() {
        let (mut dash, s) = dash_with_sample();
        let l = Layout::new(1600.0, 900.0);
        for _ in 0..120 {
            dash.update(&s, 0.0, 1.0 / 60.0, true, Some(PanelId::Speed), None);
        }
        assert!(dash.hover_of(PanelId::Speed) > 0.9, "hover spring did not arrive");
        assert!(dash.hover_of(PanelId::GForce) < 0.05, "unrelated panel is hovered");

        let mut dl = DrawList::default();
        build(&mut dl, &dash, &l, &live(), &s, SimId::Forza, 1600.0, 900.0, 60.0, false);
        let hovered_glass = dl
            .instances
            .iter()
            .filter(|i| i.meta[0] == SHAPE_GLASS && i.meta[2] > 0.5)
            .count();
        assert_eq!(hovered_glass, 1, "exactly one panel should carry hover");
    }

    #[test]
    fn gear_labels_cover_reverse_and_neutral() {
        assert_eq!(gear_label(-1), "R");
        assert_eq!(gear_label(0), "N");
        assert_eq!(gear_label(6), "6");
    }
}
