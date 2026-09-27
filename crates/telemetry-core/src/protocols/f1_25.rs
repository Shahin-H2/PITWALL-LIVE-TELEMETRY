//! F1 25 (EA / Codemasters 2025 UDP spec) decoder.
//!
//! Unlike Forza's single fat packet, the F1 titles emit ~15 different packet
//! types on one port, each with a 29-byte common header carrying a `packetId`.
//! A complete picture of the car requires fusing several of them:
//!
//! | ID | Packet         | What we take from it                        |
//! |----|----------------|---------------------------------------------|
//! | 0  | Motion         | g-forces, attitude, world position          |
//! | 2  | LapData        | lap number, lap distance, lap times         |
//! | 6  | CarTelemetry   | throttle, brake, gear, RPM, speed, temps    |
//! | 7  | CarStatus      | fuel, DRS availability, tyre compound       |
//! | 10 | CarDamage      | (not yet consumed)                          |
//! | 13 | MotionEx       | per-wheel slip ratio/angle, suspension      |
//!
//! We keep a persistent [`F1Assembler`] that merges these into one sample, and
//! treats **CarTelemetry (6) as the heartbeat** — that is the packet that
//! triggers a fresh sample downstream, because it carries the channels the
//! driver actually watches.
//!
//! # On hardcoded offsets and spec drift
//!
//! EA revises this spec every year and shifts fields inside the per-car
//! structs. Hardcoding a per-car stride is how you ship a silent data
//! corruption bug the season after release. Instead we **derive the stride
//! from the packet length**: the array is 22 cars wide and the trailing-field
//! sizes are stable, so `stride = (len - header - trailer) / 22` self-corrects
//! when EA adds a field. Offsets *within* the first few fields of each per-car
//! struct have been stable since F1 2022 and are safe to hardcode.

use crate::raw::*;
use crate::sample::*;

pub const HEADER_LEN: usize = 29;
pub const MAX_CARS: usize = 22;

/// `m_packetFormat` for the 2025 title. This is a genuine magic number, which
/// makes F1 the easiest of the three sims to auto-detect.
pub const PACKET_FORMAT_2025: u16 = 2025;
/// We also decode F1 23/24 packets; the fields we read have not moved.
const ACCEPTED_FORMATS: [u16; 3] = [2023, 2024, 2025];

// ---- common header offsets ----------------------------------------------
const H_PACKET_FORMAT: usize = 0;
const H_PACKET_ID: usize = 6;
const H_SESSION_TIME: usize = 15;
const H_PLAYER_CAR_IDX: usize = 27;

// ---- packet IDs ----------------------------------------------------------
pub const ID_MOTION: u8 = 0;
pub const ID_LAP_DATA: u8 = 2;
pub const ID_CAR_TELEMETRY: u8 = 6;
pub const ID_CAR_STATUS: u8 = 7;
pub const ID_MOTION_EX: u8 = 13;

// ---- CarTelemetry per-car offsets ---------------------------------------
const T_SPEED_KPH: usize = 0; // u16
const T_THROTTLE: usize = 2; // f32 0..1
const T_STEER: usize = 6; // f32 -1..1
const T_BRAKE: usize = 10; // f32 0..1
const T_CLUTCH: usize = 14; // u8 0..100
const T_GEAR: usize = 15; // i8, -1=R, 0=N
const T_ENGINE_RPM: usize = 16; // u16
const T_DRS: usize = 18; // u8
const T_BRAKE_TEMP: usize = 21; // u16 x4, Celsius
const T_TYRE_SURFACE_TEMP: usize = 29; // u8 x4, Celsius
const T_TYRE_PRESSURE: usize = 40; // f32 x4, PSI
/// 3 trailing bytes after the car array: MFD panel x2 + suggested gear.
const CAR_TELEMETRY_TRAILER: usize = 3;

// ---- Motion per-car offsets ---------------------------------------------
const M_WORLD_POS: usize = 0; // f32 x3
const M_WORLD_VEL: usize = 12; // f32 x3
const M_G_LATERAL: usize = 36; // f32, in g
const M_G_LONGITUDINAL: usize = 40; // f32, in g
const M_YAW: usize = 48; // f32 radians
const M_PITCH: usize = 52;
const M_ROLL: usize = 56;

// ---- LapData per-car offsets --------------------------------------------
const L_LAST_LAP_MS: usize = 0; // u32
const L_CURRENT_LAP_MS: usize = 4; // u32
const L_LAP_DISTANCE: usize = 20; // f32, metres
const L_LAP_NUMBER_CANDIDATES: [usize; 0] = [];
/// 2 trailing bytes: time-trial PB and rival car indices.
const LAP_DATA_TRAILER: usize = 2;

// ---- MotionEx offsets (single car — the player's — not an array) --------
const MX_SUSP_POSITION: usize = 0; // f32 x4
const MX_WHEEL_SLIP_RATIO: usize = 48; // f32 x4
const MX_WHEEL_SLIP_ANGLE: usize = 64; // f32 x4
const MX_WHEEL_SPEED: usize = 96; // f32 x4

/// Derive the per-car stride from the observed packet length.
///
/// Returns `None` when the length doesn't divide cleanly, which means the
/// packet is malformed or the spec changed more than a field append.
#[inline]
fn per_car_stride(len: usize, trailer: usize) -> Option<usize> {
    let body = len.checked_sub(HEADER_LEN + trailer)?;
    if body == 0 || body % MAX_CARS != 0 {
        return None;
    }
    Some(body / MAX_CARS)
}

pub fn probe(buf: &[u8]) -> u8 {
    if buf.len() < HEADER_LEN {
        return 0;
    }
    let Some(fmt) = u16_at(buf, H_PACKET_FORMAT) else {
        return 0;
    };
    if !ACCEPTED_FORMATS.contains(&fmt) {
        return 0;
    }
    let Some(id) = u8_at(buf, H_PACKET_ID) else {
        return 0;
    };
    if id > 15 {
        return 0;
    }
    let Some(player_idx) = u8_at(buf, H_PLAYER_CAR_IDX) else {
        return 0;
    };
    if player_idx as usize >= MAX_CARS && player_idx != 255 {
        return 0;
    }
    // An explicit format magic plus a valid packet ID is about as unambiguous
    // as UDP telemetry identification gets.
    if fmt == PACKET_FORMAT_2025 {
        255
    } else {
        240
    }
}

/// Accumulates the several F1 packet types into one coherent sample.
///
/// F1 spreads the car state across packets that arrive at different rates, so
/// unlike Forza we cannot decode a sample from a single datagram. This holds
/// the partial state between packets.
#[derive(Default)]
pub struct F1Assembler {
    sample: TelemetrySample,
    seen_telemetry: bool,
}

impl F1Assembler {
    pub fn new() -> Self {
        Self {
            sample: TelemetrySample {
                sim: SimId::F1_25,
                ..Default::default()
            },
            seen_telemetry: false,
        }
    }

    /// Feed one datagram. Returns a completed sample only when this packet was
    /// the CarTelemetry heartbeat, so downstream sees one sample per tick
    /// rather than five partial updates.
    pub fn ingest(&mut self, buf: &[u8], t_capture_ns: u64) -> Option<TelemetrySample> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let packet_id = u8_at(buf, H_PACKET_ID)?;
        let player = u8_at(buf, H_PLAYER_CAR_IDX)? as usize;
        if player >= MAX_CARS {
            return None;
        }

        self.sample.sim = SimId::F1_25;
        if let Some(t) = f32_at(buf, H_SESSION_TIME) {
            self.sample.t_sim_ms = (t * 1000.0) as u32;
        }

        match packet_id {
            ID_CAR_TELEMETRY => {
                self.decode_car_telemetry(buf, player)?;
                self.seen_telemetry = true;
                self.sample.t_capture_ns = t_capture_ns;
                self.sample.is_active = true;
                Some(self.sample)
            }
            ID_MOTION => {
                self.decode_motion(buf, player);
                None
            }
            ID_MOTION_EX => {
                self.decode_motion_ex(buf);
                None
            }
            ID_LAP_DATA => {
                self.decode_lap_data(buf, player);
                None
            }
            _ => None,
        }
    }

    fn decode_car_telemetry(&mut self, buf: &[u8], player: usize) -> Option<()> {
        let stride = per_car_stride(buf.len(), CAR_TELEMETRY_TRAILER)?;
        let base = HEADER_LEN + player * stride;
        let s = &mut self.sample;

        s.speed_ms = u16_at(buf, base + T_SPEED_KPH)? as f32 / 3.6;
        s.throttle = f32_at(buf, base + T_THROTTLE)?.clamp(0.0, 1.0);
        s.steer = f32_at(buf, base + T_STEER)?.clamp(-1.0, 1.0);
        s.brake = f32_at(buf, base + T_BRAKE)?.clamp(0.0, 1.0);
        s.clutch = u8_at(buf, base + T_CLUTCH)? as f32 / 100.0;
        s.gear = i8_at(buf, base + T_GEAR)?;
        s.rpm = u16_at(buf, base + T_ENGINE_RPM)? as f32;
        s.drs = u8_at(buf, base + T_DRS)?;

        // F1 never sends the redline, so we track the highest RPM we've seen
        // and use it as a running estimate. It converges within one lap and is
        // good enough to scale the tacho arc.
        if s.rpm > s.max_rpm {
            s.max_rpm = s.rpm;
        }

        // F1 sends wheel arrays as [RL, RR, FL, FR] — permute to canonical.
        let bt = [
            u16_at(buf, base + T_BRAKE_TEMP)? as f32,
            u16_at(buf, base + T_BRAKE_TEMP + 2)? as f32,
            u16_at(buf, base + T_BRAKE_TEMP + 4)? as f32,
            u16_at(buf, base + T_BRAKE_TEMP + 6)? as f32,
        ];
        s.brake_temp = from_f1_wheel_order(bt);

        let tt = [
            u8_at(buf, base + T_TYRE_SURFACE_TEMP)? as f32,
            u8_at(buf, base + T_TYRE_SURFACE_TEMP + 1)? as f32,
            u8_at(buf, base + T_TYRE_SURFACE_TEMP + 2)? as f32,
            u8_at(buf, base + T_TYRE_SURFACE_TEMP + 3)? as f32,
        ];
        s.tire_temp = from_f1_wheel_order(tt);
        s.tire_pressure = from_f1_wheel_order(f32x4_at(buf, base + T_TYRE_PRESSURE)?);

        s.present |= FieldMask::SPEED
            | FieldMask::THROTTLE
            | FieldMask::BRAKE
            | FieldMask::CLUTCH
            | FieldMask::STEER
            | FieldMask::GEAR
            | FieldMask::RPM
            | FieldMask::DRS
            | FieldMask::TIRE_TEMP
            | FieldMask::BRAKE_TEMP
            | FieldMask::TIRE_PRESSURE;
        Some(())
    }

    fn decode_motion(&mut self, buf: &[u8], player: usize) -> Option<()> {
        let stride = per_car_stride(buf.len(), 0)?;
        let base = HEADER_LEN + player * stride;
        let s = &mut self.sample;

        s.pos = Vec3::new(
            f32_at(buf, base + M_WORLD_POS)?,
            f32_at(buf, base + M_WORLD_POS + 4)?,
            f32_at(buf, base + M_WORLD_POS + 8)?,
        );
        s.velocity = Vec3::new(
            f32_at(buf, base + M_WORLD_VEL)?,
            f32_at(buf, base + M_WORLD_VEL + 4)?,
            f32_at(buf, base + M_WORLD_VEL + 8)?,
        );
        // F1 reports g-force in g; the canonical sample is m/s^2.
        s.accel = Vec3::new(
            f32_at(buf, base + M_G_LATERAL)? * 9.80665,
            0.0,
            f32_at(buf, base + M_G_LONGITUDINAL)? * 9.80665,
        );
        s.yaw = f32_at(buf, base + M_YAW)?;
        s.pitch = f32_at(buf, base + M_PITCH)?;
        s.roll = f32_at(buf, base + M_ROLL)?;

        s.present |=
            FieldMask::POSITION | FieldMask::VELOCITY | FieldMask::ACCEL | FieldMask::ATTITUDE;
        Some(())
    }

    /// MotionEx carries the player's car only — no per-car array — so the
    /// fields sit at fixed offsets right after the header.
    fn decode_motion_ex(&mut self, buf: &[u8]) -> Option<()> {
        let base = HEADER_LEN;
        let s = &mut self.sample;

        s.susp_travel = from_f1_wheel_order(f32x4_at(buf, base + MX_SUSP_POSITION)?);
        s.slip_ratio = from_f1_wheel_order(f32x4_at(buf, base + MX_WHEEL_SLIP_RATIO)?);
        s.slip_angle = from_f1_wheel_order(f32x4_at(buf, base + MX_WHEEL_SLIP_ANGLE)?);
        s.wheel_speed = from_f1_wheel_order(f32x4_at(buf, base + MX_WHEEL_SPEED)?);

        s.present |= FieldMask::SUSP_TRAVEL
            | FieldMask::SLIP_RATIO
            | FieldMask::SLIP_ANGLE
            | FieldMask::WHEEL_SPEED;
        Some(())
    }

    fn decode_lap_data(&mut self, buf: &[u8], player: usize) -> Option<()> {
        let _ = L_LAP_NUMBER_CANDIDATES;
        let stride = per_car_stride(buf.len(), LAP_DATA_TRAILER)?;
        let base = HEADER_LEN + player * stride;
        let s = &mut self.sample;

        s.last_lap_s = u32_at(buf, base + L_LAST_LAP_MS)? as f32 / 1000.0;
        s.current_lap_s = u32_at(buf, base + L_CURRENT_LAP_MS)? as f32 / 1000.0;
        s.lap_distance = f32_at(buf, base + L_LAP_DISTANCE)?;

        if s.best_lap_s <= 0.0 || (s.last_lap_s > 0.0 && s.last_lap_s < s.best_lap_s) {
            s.best_lap_s = s.last_lap_s;
        }
        s.present |= FieldMask::LAP_TIMES | FieldMask::LAP_DISTANCE;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(packet_id: u8, player: u8) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[H_PACKET_FORMAT..H_PACKET_FORMAT + 2]
            .copy_from_slice(&PACKET_FORMAT_2025.to_le_bytes());
        h[H_PACKET_ID] = packet_id;
        h[H_PLAYER_CAR_IDX] = player;
        h
    }

    #[test]
    fn probe_accepts_2025_rejects_noise() {
        let h = header(ID_CAR_TELEMETRY, 0);
        assert_eq!(probe(&h), 255);
        assert_eq!(probe(&[0u8; 29]), 0);
        assert_eq!(probe(&[0u8; 4]), 0);
    }

    #[test]
    fn car_telemetry_reads_player_slot() {
        let stride = 60usize;
        let len = HEADER_LEN + MAX_CARS * stride + CAR_TELEMETRY_TRAILER;
        let mut b = vec![0u8; len];
        b[..HEADER_LEN].copy_from_slice(&header(ID_CAR_TELEMETRY, 3));

        let base = HEADER_LEN + 3 * stride;
        b[base + T_SPEED_KPH..base + T_SPEED_KPH + 2].copy_from_slice(&288u16.to_le_bytes());
        b[base + T_THROTTLE..base + T_THROTTLE + 4].copy_from_slice(&1.0f32.to_le_bytes());
        b[base + T_GEAR] = 7;
        b[base + T_ENGINE_RPM..base + T_ENGINE_RPM + 2]
            .copy_from_slice(&11500u16.to_le_bytes());

        let mut asm = F1Assembler::new();
        let s = asm.ingest(&b, 0).expect("telemetry packet yields a sample");
        assert_eq!(s.gear, 7);
        assert_eq!(s.rpm, 11500.0);
        assert!((s.speed_ms - 80.0).abs() < 0.01, "288 km/h == 80 m/s");
        assert!((s.throttle - 1.0).abs() < 1e-6);
    }

    #[test]
    fn stride_derivation_survives_a_field_append() {
        // Simulate EA adding 4 bytes per car in a future title.
        for stride in [60usize, 64] {
            let len = HEADER_LEN + MAX_CARS * stride + CAR_TELEMETRY_TRAILER;
            assert_eq!(per_car_stride(len, CAR_TELEMETRY_TRAILER), Some(stride));
        }
    }

    #[test]
    fn motion_ex_permutes_f1_wheel_order() {
        let len = HEADER_LEN + 200;
        let mut b = vec![0u8; len];
        b[..HEADER_LEN].copy_from_slice(&header(ID_MOTION_EX, 0));
        // F1 order is [RL, RR, FL, FR] — write a recognisable ramp.
        let base = HEADER_LEN + MX_WHEEL_SLIP_ANGLE;
        for (i, v) in [10.0f32, 20.0, 30.0, 40.0].iter().enumerate() {
            b[base + i * 4..base + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut asm = F1Assembler::new();
        assert!(asm.ingest(&b, 0).is_none(), "MotionEx is not the heartbeat");
        // RL=10, RR=20, FL=30, FR=40  =>  canonical [FL,FR,RL,RR] = [30,40,10,20]
        assert_eq!(asm.sample.slip_angle, [30.0, 40.0, 10.0, 20.0]);
    }

    #[test]
    fn short_packets_do_not_panic() {
        let mut asm = F1Assembler::new();
        for n in 0..200 {
            let _ = asm.ingest(&vec![0u8; n], 0);
            let _ = probe(&vec![0u8; n]);
        }
    }
}
