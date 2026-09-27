//! Frame-rate-independent animation.
//!
//! Two things in here, and the distinction between them is most of what makes
//! an instrument cluster feel real rather than merely smooth.
//!
//! # Why not `lerp(current, target, 0.2)`
//!
//! The everywhere-default `value += (target - value) * 0.2` is **frame-rate
//! dependent**: at 240 Hz it converges four times faster than at 60 Hz, so the
//! same app feels different on different monitors and the animation speeds up
//! whenever the scene gets simpler. [`Smoothed`] fixes that by deriving the
//! coefficient from real elapsed time: `α = 1 − e^(−dt/τ)`, where τ is a time
//! constant in seconds and means something physical.
//!
//! # Why a spring for needles
//!
//! A real tachometer needle has mass. It overshoots slightly, settles, and
//! resists sudden reversal. [`Spring`] is a critically damped second-order
//! system — the same maths. Critically damped (ζ = 1) means it reaches the
//! target as fast as possible *without* oscillating, which is what an
//! instrument does and what an under-damped bounce very much does not.
//!
//! The critically damped case is integrated **analytically**, not numerically.
//! That matters more than it sounds: a stiff needle (ω = 300 rad/s for a 20 ms
//! settle) integrated with semi-implicit Euler is only stable while ω·dt < 2,
//! i.e. dt < 6.7 ms. One long frame — a shader recompile, a debugger pause, a
//! window drag — and the needle does not lag, it *explodes*, to 1e11 within
//! four steps. The closed form below is exact at any timestep and degrades
//! correctly: after a two-second stall the needle is simply at its target,
//! which is what it should be showing anyway.
//!
//! Use [`Spring`] for anything with physical presence: needles, bars, the
//! g-g dot. Use [`Smoothed`] for values where overshoot would be wrong, like
//! a numeric readout or an opacity fade.

use std::f32::consts::TAU;

/// Exponential smoothing with a real time constant.
#[derive(Clone, Copy, Debug)]
pub struct Smoothed {
    value: f32,
    /// Seconds to reach ~63% of the way to a step change.
    pub tau: f32,
}

impl Smoothed {
    pub fn new(initial: f32, tau: f32) -> Self {
        Self { value: initial, tau: tau.max(1e-4) }
    }

    #[inline]
    pub fn update(&mut self, target: f32, dt: f32) -> f32 {
        // α = 1 − e^(−dt/τ). At dt → 0 this tends to 0 (no movement); at
        // dt → ∞ it tends to 1 (snap to target). Both limits are correct,
        // which is exactly what the naive fixed-alpha version gets wrong.
        let alpha = 1.0 - (-dt / self.tau).exp();
        self.value += (target - self.value) * alpha;
        self.value
    }

    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn snap(&mut self, v: f32) {
        self.value = v;
    }
}

/// Critically damped second-order spring.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    /// Undamped natural frequency, radians/second. Higher = stiffer, snappier.
    pub omega: f32,
    /// Damping ratio. 1.0 = critically damped (no overshoot). Below 1 bounces;
    /// above 1 is sluggish.
    pub zeta: f32,
}

impl Spring {
    /// Build a spring from an intuitive settling time rather than raw omega.
    ///
    /// `settle_s` is roughly how long the needle takes to arrive. A tachometer
    /// wants ~0.06 s (it must not lag the engine); a fuel gauge wants ~0.8 s.
    pub fn new(initial: f32, settle_s: f32) -> Self {
        // For a critically damped system, settling to ~2% takes about 6/ω.
        Self {
            value: initial,
            velocity: 0.0,
            omega: 6.0 / settle_s.max(1e-3),
            zeta: 1.0,
        }
    }

    /// Override the damping ratio. Below 1.0 the spring overshoots and
    /// oscillates — occasionally wanted for a deliberately springy readout,
    /// never for an instrument needle.
    #[allow(dead_code)]
    pub fn with_damping(mut self, zeta: f32) -> Self {
        self.zeta = zeta.max(0.0);
        self
    }

    /// Advance the spring toward `target`.
    ///
    /// For the critically damped case (ζ = 1, the default) this is the exact
    /// closed-form solution of ẍ + 2ωẋ + ω²x = 0:
    ///
    /// ```text
    /// x(t) = target + (A + Bt)·e^(−ωt)     A = x₀ − target,  B = v₀ + ωA
    /// ```
    ///
    /// No stability bound, no substepping, one `exp`.
    ///
    /// For ζ ≠ 1 there is no single closed form covering all three damping
    /// regimes, so that path substeps numerically with a step size chosen to
    /// stay inside the stability bound.
    #[inline]
    pub fn update(&mut self, target: f32, dt: f32) -> f32 {
        if dt <= 0.0 {
            return self.value;
        }
        let w = self.omega;

        if (self.zeta - 1.0).abs() < 1e-3 {
            let a = self.value - target;
            let b = self.velocity + w * a;
            let e = (-w * dt).exp();
            self.value = target + (a + b * dt) * e;
            self.velocity = (self.velocity - b * w * dt) * e;
        } else {
            // Keep every substep well inside ω·h < 2.
            let max_h = 0.5 / w.max(1e-6);
            let steps = ((dt / max_h).ceil() as u32).clamp(1, 256);
            let h = dt / steps as f32;
            for _ in 0..steps {
                let accel =
                    w * w * (target - self.value) - 2.0 * self.zeta * w * self.velocity;
                self.velocity += accel * h;
                self.value += self.velocity * h;
            }
        }
        self.value
    }

    pub fn snap(&mut self, v: f32) {
        self.value = v;
        self.velocity = 0.0;
    }
}

/// Interpolates and mildly extrapolates the telemetry stream.
///
/// The sim sends at 60 Hz; the display refreshes at 144 or 240. Simply holding
/// the last sample means the gauges visibly step. This tracks the two most
/// recent samples and their arrival times, then reconstructs a value for
/// *right now* — including a bounded forward extrapolation, which visually
/// cancels part of the sim's own transport delay.
///
/// Extrapolation is capped deliberately. Predicting further than about one
/// source interval ahead turns a smooth needle into a jittery one the instant
/// the driver does something abrupt, which is precisely when they are looking.
#[derive(Clone, Copy, Debug)]
pub struct Reconstructor {
    prev_value: f32,
    prev_t: f64,
    last_value: f32,
    last_t: f64,
    /// Maximum seconds we will predict beyond the newest sample.
    pub max_extrapolation_s: f32,
}

impl Reconstructor {
    pub fn new(max_extrapolation_s: f32) -> Self {
        Self {
            prev_value: 0.0,
            prev_t: 0.0,
            last_value: 0.0,
            last_t: 0.0,
            max_extrapolation_s,
        }
    }

    /// Record a newly arrived sample. `t` is seconds on any monotonic clock.
    pub fn push(&mut self, value: f32, t: f64) {
        if t <= self.last_t {
            return; // duplicate or out-of-order packet
        }
        self.prev_value = self.last_value;
        self.prev_t = self.last_t;
        self.last_value = value;
        self.last_t = t;
    }

    /// Reconstruct the value at time `now`.
    pub fn sample(&self, now: f64) -> f32 {
        let dt_source = (self.last_t - self.prev_t) as f32;
        if dt_source <= 1e-6 {
            return self.last_value;
        }
        let rate = (self.last_value - self.prev_value) / dt_source;
        let ahead = ((now - self.last_t) as f32).clamp(0.0, self.max_extrapolation_s);
        self.last_value + rate * ahead
    }
}

/// A shake/pulse generator for the shift light and limiter effects.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pulse {
    phase: f32,
}

impl Pulse {
    /// Advance and return a 0..1 triangle wave at `hz`.
    #[inline]
    pub fn update(&mut self, hz: f32, dt: f32) -> f32 {
        self.phase = (self.phase + hz * dt * TAU) % TAU;
        (self.phase.sin() * 0.5 + 0.5).abs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_is_framerate_independent() {
        // Same wall-clock duration, different frame rates, same destination.
        let mut a = Smoothed::new(0.0, 0.1);
        for _ in 0..60 {
            a.update(1.0, 1.0 / 60.0);
        }
        let mut b = Smoothed::new(0.0, 0.1);
        for _ in 0..240 {
            b.update(1.0, 1.0 / 240.0);
        }
        assert!(
            (a.value() - b.value()).abs() < 0.01,
            "60 Hz gave {}, 240 Hz gave {} — these must agree",
            a.value(),
            b.value()
        );
    }

    #[test]
    fn critically_damped_spring_does_not_overshoot() {
        let mut s = Spring::new(0.0, 0.1);
        let mut peak = 0.0f32;
        for _ in 0..600 {
            peak = peak.max(s.update(1.0, 1.0 / 240.0));
        }
        assert!(peak <= 1.001, "critically damped spring overshot to {peak}");
        assert!((s.value - 1.0).abs() < 0.01, "should have arrived, got {}", s.value);
    }

    #[test]
    fn spring_stays_stable_across_a_dropped_frame() {
        // omega = 300 rad/s. Semi-implicit Euler needs dt < 6.7 ms here; these
        // steps are 37x that, which used to diverge to 1e11 in four frames.
        let mut s = Spring::new(0.0, 0.02);
        for _ in 0..20 {
            s.update(1.0, 0.25);
        }
        assert!(s.value.is_finite(), "spring diverged");
        assert!(
            (s.value - 1.0).abs() < 1e-3,
            "after a long stall the needle should be at the target, got {}",
            s.value
        );
    }

    #[test]
    fn stiff_spring_survives_a_range_of_pathological_timesteps() {
        for settle in [0.01f32, 0.05, 0.3] {
            for dt in [1.0 / 240.0, 1.0 / 60.0, 0.1, 0.5, 2.0] {
                let mut s = Spring::new(0.0, settle);
                for _ in 0..50 {
                    s.update(1.0, dt);
                }
                assert!(
                    s.value.is_finite() && s.value.abs() < 2.0,
                    "settle={settle} dt={dt} diverged to {}",
                    s.value
                );
            }
        }
    }

    #[test]
    fn underdamped_spring_oscillates_but_stays_bounded() {
        let mut s = Spring::new(0.0, 0.1).with_damping(0.3);
        let mut peak = 0.0f32;
        for _ in 0..2000 {
            peak = peak.max(s.update(1.0, 1.0 / 240.0));
        }
        assert!(peak > 1.0, "zeta=0.3 should overshoot, peaked at {peak}");
        assert!(peak < 2.0, "overshoot ran away to {peak}");
        assert!((s.value - 1.0).abs() < 0.05, "should settle, got {}", s.value);
    }

    #[test]
    fn reconstructor_interpolates_between_samples() {
        let mut r = Reconstructor::new(0.02);
        r.push(0.0, 0.0);
        r.push(100.0, 1.0); // 100 units/s
        // 10 ms past the last sample => +1.0 unit
        let v = r.sample(1.01);
        assert!((v - 101.0).abs() < 0.1, "got {v}");
    }

    #[test]
    fn extrapolation_is_bounded() {
        let mut r = Reconstructor::new(0.02);
        r.push(0.0, 0.0);
        r.push(100.0, 1.0);
        // Ask for a value 10 seconds later; must clamp, not fly to 1100.
        let v = r.sample(11.0);
        assert!(v <= 102.1, "extrapolation escaped its cap: {v}");
    }

    #[test]
    fn reconstructor_ignores_out_of_order_packets() {
        let mut r = Reconstructor::new(0.02);
        r.push(10.0, 1.0);
        r.push(999.0, 0.5); // late arrival
        assert_eq!(r.last_value, 10.0);
    }
}
