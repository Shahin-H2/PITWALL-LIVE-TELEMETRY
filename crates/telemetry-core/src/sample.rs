//! The canonical telemetry sample.
//!
//! Every simulator protocol decodes into exactly this type. Downstream code
//! (UI, analysis, recording) never sees a sim-specific struct.
//!
//! Design notes:
//!
//! * This is a **fixed superset struct with a presence bitmask**, not a struct
//!   of `Option<f32>`. Forza gives us no fuel data, F1 gives us no rumble-strip
//!   flags; rather than paying an enum tag per field (which would roughly double
//!   the struct and destroy cache locality) we set a bit in `present` and leave
//!   the field at its default. The struct stays `Copy` and memcpy-able, which is
//!   what lets us shove it through a lock-free triple buffer at 300 Hz.
//!
//! * **Wheel arrays are always `[FL, FR, RL, RR]`.** This matters: Forza sends
//!   that order natively, but the F1 games send `[RL, RR, FL, FR]`. The decoder
//!   is responsible for permuting into canonical order so that
//!   `slip_angle[Wheel::FrontLeft]` means the same thing on every sim.
//!
//! * Units are always SI: m/s, radians, Celsius, seconds, 0..1 for pedals.
//!   No sim's native units survive the decoder boundary.

use core::fmt;

/// Canonical wheel indices. Always `[FL, FR, RL, RR]`.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wheel {
    FrontLeft = 0,
    FrontRight = 1,
    RearLeft = 2,
    RearRight = 3,
}

impl Wheel {
    pub const ALL: [Wheel; 4] = [
        Wheel::FrontLeft,
        Wheel::FrontRight,
        Wheel::RearLeft,
        Wheel::RearRight,
    ];
    #[inline]
    pub const fn idx(self) -> usize {
        self as usize
    }
}

/// Which simulator produced a sample.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SimId {
    #[default]
    Unknown = 0,
    /// Forza Motorsport 7 / Horizon 4 / Horizon 5 / Motorsport 2023
    Forza = 1,
    /// F1 25 (EA/Codemasters 2025 spec)
    F1_25 = 2,
    /// Assetto Corsa (original) UDP remote telemetry
    AssettoCorsa = 3,
    /// Assetto Corsa Competizione (via shared-memory bridge)
    Acc = 4,
}

impl SimId {
    pub const fn name(self) -> &'static str {
        match self {
            SimId::Unknown => "Unknown",
            SimId::Forza => "Forza",
            SimId::F1_25 => "F1 25",
            SimId::AssettoCorsa => "Assetto Corsa",
            SimId::Acc => "ACC",
        }
    }
}

impl fmt::Display for SimId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Bitmask of which fields a decoder actually populated.
///
/// Check with [`TelemetrySample::has`] before trusting a field. A field whose
/// bit is clear is guaranteed to be at its `Default` value, not stale data.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct FieldMask(pub u64);

macro_rules! fields {
    ($($name:ident = $bit:expr;)*) => {
        impl FieldMask {
            $(pub const $name: FieldMask = FieldMask(1u64 << $bit);)*
        }
    };
}

fields! {
    RPM            = 0;
    GEAR           = 1;
    THROTTLE       = 2;
    BRAKE          = 3;
    CLUTCH         = 4;
    HANDBRAKE      = 5;
    STEER          = 6;
    SPEED          = 7;
    ACCEL          = 8;
    VELOCITY       = 9;
    ANG_VEL        = 10;
    ATTITUDE       = 11;
    POSITION       = 12;
    SLIP_RATIO     = 13;
    SLIP_ANGLE     = 14;
    TIRE_TEMP      = 15;
    SUSP_TRAVEL    = 16;
    WHEEL_SPEED    = 17;
    LAP_NUMBER     = 18;
    LAP_DISTANCE   = 19;
    LAP_TIMES      = 20;
    FUEL           = 21;
    DRS            = 22;
    TIRE_PRESSURE  = 23;
    BRAKE_TEMP     = 24;
    ENGINE_LIMITS  = 25;
    RIDE_HEIGHT    = 26;
}

impl FieldMask {
    #[inline]
    pub const fn union(self, other: FieldMask) -> FieldMask {
        FieldMask(self.0 | other.0)
    }
    #[inline]
    pub const fn contains(self, other: FieldMask) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl core::ops::BitOr for FieldMask {
    type Output = FieldMask;
    #[inline]
    fn bitor(self, rhs: FieldMask) -> FieldMask {
        FieldMask(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for FieldMask {
    #[inline]
    fn bitor_assign(&mut self, rhs: FieldMask) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for FieldMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FieldMask({:#018x})", self.0)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    #[inline]
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
    #[inline]
    pub fn magnitude(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }
}

/// One instant of car state, normalized across every supported simulator.
///
/// `#[repr(C)]` + `Copy` so it can be written straight into the recording file
/// and shipped through the triple buffer without serialization.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TelemetrySample {
    // ---- provenance -------------------------------------------------------
    /// Monotonic nanoseconds since app start, stamped the instant the packet
    /// came off the socket. This is *our* clock and is what all latency
    /// measurement and interpolation is based on.
    pub t_capture_ns: u64,
    /// The sim's own millisecond clock, when it provides one. Useful for
    /// detecting dropped/reordered packets; useless for latency (it's the
    /// sim's timebase, not ours).
    pub t_sim_ms: u32,
    pub sim: SimId,
    /// True when the car is actually on track and the data is meaningful.
    pub is_active: bool,
    pub present: FieldMask,

    // ---- powertrain -------------------------------------------------------
    pub rpm: f32,
    pub max_rpm: f32,
    pub idle_rpm: f32,
    /// -1 = reverse, 0 = neutral, 1..n = forward gears.
    pub gear: i8,
    /// 0..1
    pub throttle: f32,
    /// 0..1
    pub brake: f32,
    /// 0..1
    pub clutch: f32,
    /// 0..1
    pub handbrake: f32,
    /// -1..1, negative = left
    pub steer: f32,
    /// metres per second
    pub speed_ms: f32,

    // ---- motion -----------------------------------------------------------
    /// Body-frame acceleration in m/s^2.
    pub accel: Vec3,
    pub velocity: Vec3,
    pub ang_vel: Vec3,
    /// Radians.
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    /// World position in metres.
    pub pos: Vec3,

    // ---- per-wheel, always [FL, FR, RL, RR] -------------------------------
    pub slip_ratio: [f32; 4],
    /// Radians. Sign convention: positive = slipping toward the outside.
    pub slip_angle: [f32; 4],
    /// Celsius.
    pub tire_temp: [f32; 4],
    /// Metres. Positive = compression.
    pub susp_travel: [f32; 4],
    /// Radians per second.
    pub wheel_speed: [f32; 4],
    /// PSI.
    pub tire_pressure: [f32; 4],
    /// Celsius.
    pub brake_temp: [f32; 4],
    /// Metres.
    pub ride_height: [f32; 4],

    // ---- session ----------------------------------------------------------
    pub lap_number: u16,
    /// Metres travelled along the racing line this lap.
    pub lap_distance: f32,
    pub current_lap_s: f32,
    pub last_lap_s: f32,
    pub best_lap_s: f32,
    /// Kilograms or litres depending on sim; treated as relative.
    pub fuel: f32,
    pub drs: u8,
}

impl TelemetrySample {
    #[inline]
    pub fn has(&self, f: FieldMask) -> bool {
        self.present.contains(f)
    }

    /// Speed in km/h, the unit every driver actually thinks in.
    #[inline]
    pub fn speed_kmh(&self) -> f32 {
        self.speed_ms * 3.6
    }

    /// RPM as a 0..1 fraction of the usable band, for the tacho arc.
    /// Falls back gracefully when the sim didn't tell us the limits.
    #[inline]
    pub fn rpm_fraction(&self) -> f32 {
        if self.max_rpm > 1.0 {
            (self.rpm / self.max_rpm).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Longitudinal acceleration in g (positive = accelerating).
    #[inline]
    pub fn long_g(&self) -> f32 {
        self.accel.z / 9.80665
    }

    /// Lateral acceleration in g (positive = right-hand load).
    #[inline]
    pub fn lat_g(&self) -> f32 {
        self.accel.x / 9.80665
    }

    /// Understeer/oversteer balance, in radians of slip-angle difference.
    ///
    /// Positive => the rear is sliding more than the front => **oversteer**.
    /// Negative => the front is sliding more than the rear => **understeer**.
    ///
    /// This is the single most useful derived channel in the whole app: it is
    /// what a race engineer is really asking when they ask "how's the balance
    /// on entry?", and unlike the driver's opinion it is measured.
    ///
    /// Returns `None` when the sim doesn't publish per-wheel slip angles.
    #[inline]
    pub fn balance(&self) -> Option<f32> {
        if !self.has(FieldMask::SLIP_ANGLE) {
            return None;
        }
        let front = 0.5 * (self.slip_angle[Wheel::FrontLeft.idx()].abs()
            + self.slip_angle[Wheel::FrontRight.idx()].abs());
        let rear = 0.5 * (self.slip_angle[Wheel::RearLeft.idx()].abs()
            + self.slip_angle[Wheel::RearRight.idx()].abs());
        Some(rear - front)
    }

    /// Combined g-force magnitude in the horizontal plane — the driver's
    /// position on the friction ellipse.
    #[inline]
    pub fn combined_g(&self) -> f32 {
        (self.long_g().powi(2) + self.lat_g().powi(2)).sqrt()
    }
}
