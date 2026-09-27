//! Assetto Corsa (original) UDP remote-telemetry decoder.
//!
//! # AC is not passive — you must ask
//!
//! Forza and F1 fire telemetry at a configured address unprompted. Assetto
//! Corsa does not: it is **request/response**. The client sends a 12-byte
//! handshake, receives a 408-byte identity blob (car, driver, track), then
//! sends a *second* datagram subscribing to the update stream. Only then does
//! `RTCarInfo` start flowing. [`handshake_packet`] and [`subscribe_packet`]
//! build those; the ingest loop drives the state machine.
//!
//! # The bigger architectural caveat
//!
//! For **ACC** (Competizione) this UDP path does not exist in a useful form.
//! ACC's UDP "Broadcasting API" carries session and timing data, not physics.
//! The real high-rate ACC feed is a **shared-memory** mapping (physics at
//! ~333 Hz) that is only readable from the machine running the sim. If ACC
//! runs on a separate rig — which for any serious setup it will — you need a
//! small forwarder process on the sim PC that reads shared memory and relays
//! it here. That bridge is deliberately out of scope for this decoder; see
//! `docs/ARCHITECTURE.md`.

use crate::raw::*;
use crate::sample::*;

/// `RTCarInfo` wire size.
pub const RT_CAR_INFO_LEN: usize = 328;
/// `HandshakerResponse` wire size.
pub const HANDSHAKE_RESPONSE_LEN: usize = 408;

const AC_IDENTIFIER: u8 = b'a';

// Operation IDs for the outbound control datagrams.
const OP_HANDSHAKE: i32 = 0;
const OP_SUBSCRIBE_UPDATE: i32 = 1;
const OP_DISMISS: i32 = 3;

// ---- RTCarInfo offsets ---------------------------------------------------
// Note the 3 bytes of padding after `identifier` and 2 after the bool run:
// AC's struct is C-aligned, not packed, so the fields land on natural
// boundaries. Getting this wrong shifts everything after offset 20.
const O_IDENTIFIER: usize = 0;
#[allow(dead_code)] // documents the wire layout; speed is read from O_SPEED_MS
const O_SPEED_KMH: usize = 8;
const O_SPEED_MS: usize = 16;
const O_IS_IN_PIT: usize = 24;
const O_ENGINE_LIMITER_ON: usize = 25;
const O_ACC_G_VERTICAL: usize = 28;
const O_ACC_G_HORIZONTAL: usize = 32;
const O_ACC_G_FRONTAL: usize = 36;
const O_LAP_TIME_MS: usize = 40; // i32
const O_LAST_LAP_MS: usize = 44; // i32
const O_BEST_LAP_MS: usize = 48; // i32
const O_LAP_COUNT: usize = 52; // i32
const O_GAS: usize = 56;
const O_BRAKE: usize = 60;
const O_CLUTCH: usize = 64;
const O_ENGINE_RPM: usize = 68;
const O_STEER: usize = 72;
const O_GEAR: usize = 76; // i32, 0=R, 1=N, 2=1st
const O_WHEEL_ANGULAR_SPEED: usize = 84; // f32 x4
const O_SLIP_ANGLE: usize = 100; // f32 x4
const O_SLIP_RATIO: usize = 132; // f32 x4
const O_SUSPENSION_HEIGHT: usize = 292; // f32 x4
const O_CAR_POSITION_NORM: usize = 308; // f32, 0..1 around the lap
const O_CAR_COORDS: usize = 316; // f32 x3

/// Build the initial handshake datagram.
pub fn handshake_packet() -> [u8; 12] {
    control_packet(OP_HANDSHAKE)
}

/// Build the "start sending me updates" datagram, sent after the response.
pub fn subscribe_packet() -> [u8; 12] {
    control_packet(OP_SUBSCRIBE_UPDATE)
}

/// Build the polite disconnect, so AC stops transmitting when we exit.
pub fn dismiss_packet() -> [u8; 12] {
    control_packet(OP_DISMISS)
}

fn control_packet(operation_id: i32) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&1i32.to_le_bytes()); // identifier
    p[4..8].copy_from_slice(&1i32.to_le_bytes()); // version
    p[8..12].copy_from_slice(&operation_id.to_le_bytes());
    p
}

/// True when this datagram is AC's identity response rather than car data.
pub fn is_handshake_response(buf: &[u8]) -> bool {
    buf.len() == HANDSHAKE_RESPONSE_LEN
}

pub fn probe(buf: &[u8]) -> u8 {
    if buf.len() == HANDSHAKE_RESPONSE_LEN {
        return 220;
    }
    if buf.len() != RT_CAR_INFO_LEN {
        return 0;
    }
    if u8_at(buf, O_IDENTIFIER) != Some(AC_IDENTIFIER) {
        return 0;
    }
    // Guard against a coincidental 328-byte datagram whose first byte is 'a'.
    match f32_at(buf, O_ENGINE_RPM) {
        Some(rpm) if rpm.is_finite() && (0.0..=25_000.0).contains(&rpm) => 230,
        _ => 0,
    }
}

pub fn decode(buf: &[u8], t_capture_ns: u64) -> Option<TelemetrySample> {
    if buf.len() < RT_CAR_INFO_LEN || u8_at(buf, O_IDENTIFIER)? != AC_IDENTIFIER {
        return None;
    }

    let mut s = TelemetrySample {
        t_capture_ns,
        sim: SimId::AssettoCorsa,
        ..Default::default()
    };

    s.speed_ms = f32_at(buf, O_SPEED_MS)?;
    s.rpm = f32_at(buf, O_ENGINE_RPM)?;
    s.throttle = f32_at(buf, O_GAS)?.clamp(0.0, 1.0);
    s.brake = f32_at(buf, O_BRAKE)?.clamp(0.0, 1.0);
    s.clutch = f32_at(buf, O_CLUTCH)?.clamp(0.0, 1.0);
    s.steer = f32_at(buf, O_STEER)?;

    // AC: 0 = reverse, 1 = neutral, 2 = first. Canonical: -1 = R, 0 = N, 1 = 1st.
    s.gear = (i32_at(buf, O_GEAR)? - 1).clamp(-1, 10) as i8;

    // AC reports g in units of g already; canonical is m/s^2.
    s.accel = Vec3::new(
        f32_at(buf, O_ACC_G_HORIZONTAL)? * 9.80665,
        f32_at(buf, O_ACC_G_VERTICAL)? * 9.80665,
        f32_at(buf, O_ACC_G_FRONTAL)? * 9.80665,
    );

    s.pos = Vec3::new(
        f32_at(buf, O_CAR_COORDS)?,
        f32_at(buf, O_CAR_COORDS + 4)?,
        f32_at(buf, O_CAR_COORDS + 8)?,
    );

    // AC's wheel arrays are already [FL, FR, RL, RR].
    s.slip_angle = f32x4_at(buf, O_SLIP_ANGLE)?;
    s.slip_ratio = f32x4_at(buf, O_SLIP_RATIO)?;
    s.wheel_speed = f32x4_at(buf, O_WHEEL_ANGULAR_SPEED)?;
    s.ride_height = f32x4_at(buf, O_SUSPENSION_HEIGHT)?;

    s.current_lap_s = i32_at(buf, O_LAP_TIME_MS)? as f32 / 1000.0;
    s.last_lap_s = i32_at(buf, O_LAST_LAP_MS)? as f32 / 1000.0;
    s.best_lap_s = i32_at(buf, O_BEST_LAP_MS)? as f32 / 1000.0;
    s.lap_number = i32_at(buf, O_LAP_COUNT)?.clamp(0, u16::MAX as i32) as u16;

    // AC gives normalized lap position (0..1) rather than metres. We keep it
    // in `lap_distance` and let the analysis layer scale by track length.
    s.lap_distance = f32_at(buf, O_CAR_POSITION_NORM)?;

    let in_pit = u8_at(buf, O_IS_IN_PIT)? != 0;
    let limiter = u8_at(buf, O_ENGINE_LIMITER_ON)? != 0;
    s.is_active = !in_pit && s.rpm > 0.0;
    let _ = limiter;

    s.present |= FieldMask::SPEED
        | FieldMask::RPM
        | FieldMask::THROTTLE
        | FieldMask::BRAKE
        | FieldMask::CLUTCH
        | FieldMask::STEER
        | FieldMask::GEAR
        | FieldMask::ACCEL
        | FieldMask::POSITION
        | FieldMask::SLIP_ANGLE
        | FieldMask::SLIP_RATIO
        | FieldMask::WHEEL_SPEED
        | FieldMask::RIDE_HEIGHT
        | FieldMask::LAP_TIMES
        | FieldMask::LAP_NUMBER
        | FieldMask::LAP_DISTANCE;

    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac_packet() -> Vec<u8> {
        let mut b = vec![0u8; RT_CAR_INFO_LEN];
        b[O_IDENTIFIER] = AC_IDENTIFIER;
        b[O_SPEED_MS..O_SPEED_MS + 4].copy_from_slice(&62.5f32.to_le_bytes());
        b[O_ENGINE_RPM..O_ENGINE_RPM + 4].copy_from_slice(&7800.0f32.to_le_bytes());
        b[O_GAS..O_GAS + 4].copy_from_slice(&0.85f32.to_le_bytes());
        b[O_GEAR..O_GEAR + 4].copy_from_slice(&5i32.to_le_bytes()); // AC 5 == 4th
        b
    }

    #[test]
    fn gear_is_rebased_off_by_one() {
        let s = decode(&ac_packet(), 0).unwrap();
        assert_eq!(s.gear, 4, "AC's 5 is canonical 4th gear");

        let mut b = ac_packet();
        b[O_GEAR..O_GEAR + 4].copy_from_slice(&0i32.to_le_bytes());
        assert_eq!(decode(&b, 0).unwrap().gear, -1, "AC 0 is reverse");

        b[O_GEAR..O_GEAR + 4].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(decode(&b, 0).unwrap().gear, 0, "AC 1 is neutral");
    }

    #[test]
    fn probe_requires_identifier_byte() {
        assert!(probe(&ac_packet()) > 0);
        let mut b = ac_packet();
        b[O_IDENTIFIER] = b'z';
        assert_eq!(probe(&b), 0);
    }

    #[test]
    fn control_packets_are_well_formed() {
        let h = handshake_packet();
        assert_eq!(h.len(), 12);
        assert_eq!(i32::from_le_bytes(h[8..12].try_into().unwrap()), OP_HANDSHAKE);
        let s = subscribe_packet();
        assert_eq!(
            i32::from_le_bytes(s[8..12].try_into().unwrap()),
            OP_SUBSCRIBE_UPDATE
        );
    }

    #[test]
    fn short_packet_does_not_panic() {
        for n in 0..340 {
            let _ = decode(&vec![0u8; n], 0);
            let _ = probe(&vec![0u8; n]);
        }
    }
}
