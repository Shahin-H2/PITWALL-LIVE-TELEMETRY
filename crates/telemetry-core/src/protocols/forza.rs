//! Forza "Data Out" decoder — Motorsport 7, Horizon 4/5, Motorsport (2023).
//!
//! # Packet layout, and the trap that bites everyone
//!
//! Forza sends a fixed "Sled" block of 232 bytes, optionally followed by a
//! 79-byte "Dash" block. Four wire sizes exist:
//!
//! | Bytes | Title                        | Dash block starts at |
//! |-------|------------------------------|----------------------|
//! | 232   | FM7 "Sled" mode              | (no dash block)      |
//! | 311   | FM7 "Car Dash" mode          | 232                  |
//! | 324   | Horizon 4 / Horizon 5        | **244**              |
//! | 331   | Motorsport (2023)            | 232                  |
//!
//! **The Horizon titles insert 12 undocumented bytes between the Sled and Dash
//! blocks.** Every field after offset 232 therefore shifts by 12 in Horizon.
//! This is the origin of the classic "my gear readout shows 255" bug: at the
//! FM7 gear offset (307, or 315 in a hand-rolled Horizon struct that only
//! partially accounts for the shift) Horizon is actually sending the throttle
//! byte, which sweeps 0-255 with the driver's right foot instead of stepping
//! 1..8 with the shifter.
//!
//! We handle this by deriving `dash_base` from the packet length rather than
//! hardcoding offsets, so all four variants decode from one offset table.

use crate::raw::*;
use crate::sample::*;

// ---- Sled block, absolute offsets (identical in every variant) ------------
const O_IS_RACE_ON: usize = 0;
const O_TIMESTAMP_MS: usize = 4;
const O_ENGINE_MAX_RPM: usize = 8;
const O_ENGINE_IDLE_RPM: usize = 12;
const O_CURRENT_RPM: usize = 16;
const O_ACCEL: usize = 20; // X, Y, Z
const O_VELOCITY: usize = 32; // X, Y, Z
const O_ANG_VEL: usize = 44; // X, Y, Z
const O_YAW: usize = 56;
const O_PITCH: usize = 60;
const O_ROLL: usize = 64;
#[allow(dead_code)] // documents the wire layout; we use O_SUSP_TRAVEL_M instead
const O_NORM_SUSP_TRAVEL: usize = 68; // f32 x4
const O_TIRE_SLIP_RATIO: usize = 84; // f32 x4
const O_WHEEL_ROT_SPEED: usize = 100; // f32 x4
const O_TIRE_SLIP_ANGLE: usize = 164; // f32 x4
const O_SUSP_TRAVEL_M: usize = 196; // f32 x4

const SLED_LEN: usize = 232;

// ---- Dash block, offsets RELATIVE to dash_base ---------------------------
const D_POSITION: usize = 0; // X, Y, Z
const D_SPEED: usize = 12; // m/s
const D_TIRE_TEMP: usize = 24; // f32 x4, Fahrenheit
const D_FUEL: usize = 44;
const D_DIST_TRAVELED: usize = 48;
const D_BEST_LAP: usize = 52;
const D_LAST_LAP: usize = 56;
const D_CURRENT_LAP: usize = 60;
const D_LAP_NUMBER: usize = 68; // u16
const D_ACCEL: usize = 71; // u8, 0..255
const D_BRAKE: usize = 72; // u8, 0..255
const D_CLUTCH: usize = 73; // u8
const D_HANDBRAKE: usize = 74; // u8
const D_GEAR: usize = 75; // u8  <- 319 absolute in Horizon, 307 in FM7
const D_STEER: usize = 76; // i8, -127..127

const DASH_LEN: usize = 79;

/// Where the Dash block starts for a given packet length, or `None` if this
/// packet is Sled-only (or not Forza at all).
#[inline]
fn dash_base(len: usize) -> Option<usize> {
    match len {
        311 | 331 => Some(SLED_LEN),
        // Horizon 4/5: 12 unknown bytes wedged in after the sled block.
        323 | 324 => Some(SLED_LEN + 12),
        _ => None,
    }
}

/// Confidence that this buffer is a Forza packet.
///
/// Forza has no magic number, so we identify it structurally: an exact length
/// match plus `isRaceOn` being a clean boolean. That second check is what stops
/// us mis-claiming an unrelated 324-byte datagram.
pub fn probe(buf: &[u8]) -> u8 {
    let len = buf.len();
    if !matches!(len, SLED_LEN | 311 | 323 | 324 | 331) {
        return 0;
    }
    match i32_at(buf, O_IS_RACE_ON) {
        Some(0) | Some(1) => {}
        _ => return 0,
    }
    // Sanity-check the engine limits. Every real car has a redline in a
    // plausible band; garbage here means we matched a length by coincidence.
    let max_rpm = match f32_at(buf, O_ENGINE_MAX_RPM) {
        Some(v) => v,
        None => return 0,
    };
    if !(max_rpm.is_finite() && (0.0..=30_000.0).contains(&max_rpm)) {
        return 0;
    }
    // Length + boolean + plausible redline is a strong structural match.
    if dash_base(len).is_some() {
        200
    } else {
        150 // sled-only: valid, but far less useful
    }
}

pub fn decode(buf: &[u8], t_capture_ns: u64) -> Option<TelemetrySample> {
    // Enforce exactly the lengths `probe` accepts. A bare `len >= SLED_LEN`
    // check looks harmless and is not: any larger datagram — an F1 packet at
    // 1352 bytes, say — would decode "successfully" into sled-shaped garbage,
    // and because the registry treats a successful decode as confirmation
    // that it picked the right protocol, it would never re-detect. The
    // decoder must be exactly as strict as the prober.
    if !matches!(buf.len(), SLED_LEN | 311 | 323 | 324 | 331) {
        return None;
    }

    let mut s = TelemetrySample {
        t_capture_ns,
        sim: SimId::Forza,
        ..Default::default()
    };

    s.is_active = i32_at(buf, O_IS_RACE_ON)? == 1;
    s.t_sim_ms = u32_at(buf, O_TIMESTAMP_MS)?;

    s.max_rpm = f32_at(buf, O_ENGINE_MAX_RPM)?;
    s.idle_rpm = f32_at(buf, O_ENGINE_IDLE_RPM)?;
    s.rpm = f32_at(buf, O_CURRENT_RPM)?;
    s.present |= FieldMask::RPM | FieldMask::ENGINE_LIMITS;

    s.accel = Vec3::new(
        f32_at(buf, O_ACCEL)?,
        f32_at(buf, O_ACCEL + 4)?,
        f32_at(buf, O_ACCEL + 8)?,
    );
    s.velocity = Vec3::new(
        f32_at(buf, O_VELOCITY)?,
        f32_at(buf, O_VELOCITY + 4)?,
        f32_at(buf, O_VELOCITY + 8)?,
    );
    s.ang_vel = Vec3::new(
        f32_at(buf, O_ANG_VEL)?,
        f32_at(buf, O_ANG_VEL + 4)?,
        f32_at(buf, O_ANG_VEL + 8)?,
    );
    s.present |= FieldMask::ACCEL | FieldMask::VELOCITY | FieldMask::ANG_VEL;

    s.yaw = f32_at(buf, O_YAW)?;
    s.pitch = f32_at(buf, O_PITCH)?;
    s.roll = f32_at(buf, O_ROLL)?;
    s.present |= FieldMask::ATTITUDE;

    // Forza's sled block is already [FL, FR, RL, RR] — no permutation needed.
    s.slip_ratio = f32x4_at(buf, O_TIRE_SLIP_RATIO)?;
    s.slip_angle = f32x4_at(buf, O_TIRE_SLIP_ANGLE)?;
    s.wheel_speed = f32x4_at(buf, O_WHEEL_ROT_SPEED)?;
    s.susp_travel = f32x4_at(buf, O_SUSP_TRAVEL_M)?;
    s.present |= FieldMask::SLIP_RATIO
        | FieldMask::SLIP_ANGLE
        | FieldMask::WHEEL_SPEED
        | FieldMask::SUSP_TRAVEL;

    // Fall back to integrating the velocity vector when there's no dash block.
    s.speed_ms = s.velocity.magnitude();
    s.present |= FieldMask::SPEED;

    let Some(db) = dash_base(buf.len()) else {
        // Sled-only packet: everything above is valid, pedals/gear are not.
        return Some(s);
    };
    if buf.len() < db + DASH_LEN {
        return Some(s);
    }

    s.pos = Vec3::new(
        f32_at(buf, db + D_POSITION)?,
        f32_at(buf, db + D_POSITION + 4)?,
        f32_at(buf, db + D_POSITION + 8)?,
    );
    s.present |= FieldMask::POSITION;

    s.speed_ms = f32_at(buf, db + D_SPEED)?;

    // Forza reports tyre temperature in Fahrenheit. Everything downstream is
    // Celsius, so the conversion happens here at the protocol boundary.
    let temp_f = f32x4_at(buf, db + D_TIRE_TEMP)?;
    for i in 0..4 {
        s.tire_temp[i] = (temp_f[i] - 32.0) * 5.0 / 9.0;
    }
    s.present |= FieldMask::TIRE_TEMP;

    s.fuel = f32_at(buf, db + D_FUEL)?;
    s.lap_distance = f32_at(buf, db + D_DIST_TRAVELED)?;
    s.best_lap_s = f32_at(buf, db + D_BEST_LAP)?;
    s.last_lap_s = f32_at(buf, db + D_LAST_LAP)?;
    s.current_lap_s = f32_at(buf, db + D_CURRENT_LAP)?;
    s.lap_number = u16_at(buf, db + D_LAP_NUMBER)?;
    s.present |= FieldMask::FUEL
        | FieldMask::LAP_DISTANCE
        | FieldMask::LAP_TIMES
        | FieldMask::LAP_NUMBER;

    s.throttle = u8_at(buf, db + D_ACCEL)? as f32 / 255.0;
    s.brake = u8_at(buf, db + D_BRAKE)? as f32 / 255.0;
    s.clutch = u8_at(buf, db + D_CLUTCH)? as f32 / 255.0;
    s.handbrake = u8_at(buf, db + D_HANDBRAKE)? as f32 / 255.0;
    s.present |=
        FieldMask::THROTTLE | FieldMask::BRAKE | FieldMask::CLUTCH | FieldMask::HANDBRAKE;

    // Forza: 0 = reverse, 1..n = forward. Canonical: -1 = reverse, 0 = neutral.
    let raw_gear = u8_at(buf, db + D_GEAR)?;
    s.gear = if raw_gear == 0 { -1 } else { raw_gear as i8 };
    s.present |= FieldMask::GEAR;

    s.steer = i8_at(buf, db + D_STEER)? as f32 / 127.0;
    s.present |= FieldMask::STEER;

    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic Horizon packet with known values at known offsets.
    fn horizon_packet() -> Vec<u8> {
        let mut b = vec![0u8; 324];
        b[O_IS_RACE_ON..O_IS_RACE_ON + 4].copy_from_slice(&1i32.to_le_bytes());
        b[O_ENGINE_MAX_RPM..O_ENGINE_MAX_RPM + 4].copy_from_slice(&7500.0f32.to_le_bytes());
        b[O_CURRENT_RPM..O_CURRENT_RPM + 4].copy_from_slice(&6200.0f32.to_le_bytes());
        let db = SLED_LEN + 12;
        b[db + D_SPEED..db + D_SPEED + 4].copy_from_slice(&50.0f32.to_le_bytes());
        b[db + D_ACCEL] = 255; // full throttle
        b[db + D_BRAKE] = 0;
        b[db + D_GEAR] = 4;
        b
    }

    #[test]
    fn decodes_horizon_gear_at_319_not_315() {
        let b = horizon_packet();
        let s = decode(&b, 0).expect("decode");
        assert_eq!(s.gear, 4, "gear must come from absolute offset 319");
        assert!((s.throttle - 1.0).abs() < 1e-6);
        assert!((s.speed_ms - 50.0).abs() < 1e-6);
    }

    /// This is the bug that was in the original engine.c: reading gear at 315
    /// picks up the throttle byte instead.
    #[test]
    fn offset_315_is_throttle_not_gear() {
        let b = horizon_packet();
        assert_eq!(b[315], 255, "byte 315 in Horizon is the throttle byte");
        assert_eq!(b[319], 4, "byte 319 in Horizon is the gear byte");
    }

    #[test]
    fn fm7_dash_uses_unshifted_base() {
        let mut b = vec![0u8; 311];
        b[O_IS_RACE_ON..O_IS_RACE_ON + 4].copy_from_slice(&1i32.to_le_bytes());
        b[O_ENGINE_MAX_RPM..O_ENGINE_MAX_RPM + 4].copy_from_slice(&7500.0f32.to_le_bytes());
        b[SLED_LEN + D_GEAR] = 3; // absolute 307
        let s = decode(&b, 0).unwrap();
        assert_eq!(s.gear, 3);
    }

    #[test]
    fn rejects_foreign_packets() {
        assert_eq!(probe(&[0u8; 100]), 0);
        // Right length, but isRaceOn is garbage.
        let mut b = vec![0u8; 324];
        b[0..4].copy_from_slice(&99i32.to_le_bytes());
        assert_eq!(probe(&b), 0);
    }

    #[test]
    fn reverse_gear_maps_to_negative_one() {
        let mut b = horizon_packet();
        b[SLED_LEN + 12 + D_GEAR] = 0;
        assert_eq!(decode(&b, 0).unwrap().gear, -1);
    }

    #[test]
    fn short_packet_does_not_panic() {
        for n in 0..340 {
            let _ = decode(&vec![0u8; n], 0);
            let _ = probe(&vec![0u8; n]);
        }
    }
}
