//! Protocol registry: identifies which simulator is talking and decodes it.
//!
//! # Why auto-detect rather than a mode switch
//!
//! The brief asked for a client window to switch protocols on the fly. That
//! exists ([`Registry::pin`]), but it is the *override*, not the primary path,
//! because every one of these protocols self-identifies:
//!
//! * **F1 25** opens with `m_packetFormat == 2025` — a real magic number.
//! * **Assetto Corsa** stamps `'a'` in byte 0 of every `RTCarInfo`.
//! * **Forza** has no magic, but the combination of an exact packet length,
//!   `isRaceOn ∈ {0,1}`, and a plausible redline is unambiguous in practice.
//!
//! Each decoder returns a confidence score rather than a bool, so when two
//! protocols could plausibly claim a datagram the strongest magic wins. A
//! driver should never have to tell the app what they are playing; they should
//! only have to *correct* it, which is what pinning is for.

use crate::protocols::{assetto, f1_25, forza};
use crate::sample::{SimId, TelemetrySample};

/// How the registry chose the active protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionMode {
    /// Chosen by probing packet content.
    Auto,
    /// Forced by the user from the client window.
    Pinned,
}

pub struct Registry {
    active: SimId,
    mode: DetectionMode,
    /// F1 needs cross-packet state; the other two are stateless.
    f1: f1_25::F1Assembler,
    /// Consecutive datagrams the active decoder has failed to decode. Used to
    /// fall back to re-detection when the driver alt-tabs into another sim.
    misses: u32,
    packets_seen: u64,
    packets_decoded: u64,
}

/// After this many undecodable packets in a row we abandon the current
/// protocol and re-probe. Sized so a brief burst of malformed or unhandled
/// packet types (F1 sends plenty we ignore) never triggers a switch.
const MISS_THRESHOLD: u32 = 120;

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            active: SimId::Unknown,
            mode: DetectionMode::Auto,
            f1: f1_25::F1Assembler::new(),
            misses: 0,
            packets_seen: 0,
            packets_decoded: 0,
        }
    }

    pub fn active(&self) -> SimId {
        self.active
    }

    pub fn mode(&self) -> DetectionMode {
        self.mode
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.packets_seen, self.packets_decoded)
    }

    /// Force a specific protocol — the manual override behind the client
    /// window's sim selector.
    pub fn pin(&mut self, sim: SimId) {
        self.active = sim;
        self.mode = DetectionMode::Pinned;
        self.misses = 0;
        if sim == SimId::F1_25 {
            self.f1 = f1_25::F1Assembler::new();
        }
    }

    /// Return to content-based detection.
    ///
    /// Clearing `active` matters: leaving the previously pinned sim in place
    /// would mean auto-detection doesn't re-run until `MISS_THRESHOLD`
    /// consecutive failures, so the user would unpin and see nothing change
    /// for two seconds. Unpinning means "you work it out", so we forget.
    pub fn unpin(&mut self) {
        self.mode = DetectionMode::Auto;
        self.active = SimId::Unknown;
        self.misses = 0;
    }

    /// Score every registered protocol against a datagram and return the best.
    pub fn identify(buf: &[u8]) -> (SimId, u8) {
        let candidates = [
            (SimId::F1_25, f1_25::probe(buf)),
            (SimId::AssettoCorsa, assetto::probe(buf)),
            (SimId::Forza, forza::probe(buf)),
        ];
        candidates
            .into_iter()
            .max_by_key(|(_, score)| *score)
            .filter(|(_, score)| *score > 0)
            .unwrap_or((SimId::Unknown, 0))
    }

    /// Feed one datagram; get a sample back when one completes.
    ///
    /// Returns `None` for packets that are valid but not a heartbeat (most of
    /// F1's packet types), and for packets we cannot identify at all.
    pub fn ingest(&mut self, buf: &[u8], t_capture_ns: u64) -> Option<TelemetrySample> {
        self.packets_seen += 1;

        if self.mode == DetectionMode::Auto
            && (self.active == SimId::Unknown || self.misses >= MISS_THRESHOLD)
        {
            let (sim, score) = Self::identify(buf);
            if score > 0 && sim != self.active {
                self.active = sim;
                self.misses = 0;
                if sim == SimId::F1_25 {
                    self.f1 = f1_25::F1Assembler::new();
                }
            }
        }

        let decoded = match self.active {
            SimId::Forza => forza::decode(buf, t_capture_ns),
            SimId::F1_25 => self.f1.ingest(buf, t_capture_ns),
            SimId::AssettoCorsa | SimId::Acc => {
                if assetto::is_handshake_response(buf) {
                    None
                } else {
                    assetto::decode(buf, t_capture_ns)
                }
            }
            SimId::Unknown => None,
        };

        match decoded {
            Some(s) => {
                self.misses = 0;
                self.packets_decoded += 1;
                Some(s)
            }
            None => {
                self.misses = self.misses.saturating_add(1);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f1_telemetry_packet() -> Vec<u8> {
        let stride = 60usize;
        let mut b = vec![0u8; f1_25::HEADER_LEN + f1_25::MAX_CARS * stride + 3];
        b[0..2].copy_from_slice(&f1_25::PACKET_FORMAT_2025.to_le_bytes());
        b[6] = f1_25::ID_CAR_TELEMETRY;
        b[27] = 0;
        b[f1_25::HEADER_LEN + 15] = 4; // gear
        b
    }

    fn forza_packet() -> Vec<u8> {
        let mut b = vec![0u8; 324];
        b[0..4].copy_from_slice(&1i32.to_le_bytes());
        b[8..12].copy_from_slice(&7500.0f32.to_le_bytes());
        b[244 + 75] = 3;
        b
    }

    #[test]
    fn identifies_each_sim_from_content_alone() {
        assert_eq!(Registry::identify(&f1_telemetry_packet()).0, SimId::F1_25);
        assert_eq!(Registry::identify(&forza_packet()).0, SimId::Forza);
        assert_eq!(Registry::identify(&[0u8; 7]).0, SimId::Unknown);
    }

    #[test]
    fn auto_detects_then_decodes() {
        let mut r = Registry::new();
        let s = r.ingest(&forza_packet(), 1).expect("sample");
        assert_eq!(r.active(), SimId::Forza);
        assert_eq!(s.gear, 3);
    }

    #[test]
    fn switching_sims_recovers_after_miss_threshold() {
        let mut r = Registry::new();
        r.ingest(&forza_packet(), 0);
        assert_eq!(r.active(), SimId::Forza);

        // Driver quits Forza and loads F1. Forza's decoder rejects the new
        // packets; after the threshold we re-probe and land on F1.
        let f1 = f1_telemetry_packet();
        let mut got = None;
        for i in 0..(MISS_THRESHOLD + 5) {
            got = r.ingest(&f1, i as u64);
        }
        assert_eq!(r.active(), SimId::F1_25);
        assert_eq!(got.map(|s| s.gear), Some(4));
    }

    #[test]
    fn pinning_defeats_auto_detection() {
        let mut r = Registry::new();
        r.pin(SimId::AssettoCorsa);
        // A valid Forza packet must NOT flip us off the pinned protocol.
        for _ in 0..(MISS_THRESHOLD + 10) {
            r.ingest(&forza_packet(), 0);
        }
        assert_eq!(r.active(), SimId::AssettoCorsa);
        assert_eq!(r.mode(), DetectionMode::Pinned);

        r.unpin();
        r.ingest(&forza_packet(), 0);
        assert_eq!(r.active(), SimId::Forza);
    }
}
