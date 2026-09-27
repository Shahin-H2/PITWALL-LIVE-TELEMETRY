//! Session recording and replay.
//!
//! This is the least glamorous module in the project and the one that will
//! save the most time. Without it, every UI tweak, every spring constant, and
//! every analysis change costs you a real lap in a real sim. With it, you
//! iterate against a captured lap in a tight loop, frame-step a corner, and
//! reproduce bugs exactly.
//!
//! # We record raw datagrams, not decoded samples
//!
//! Deliberately. Storing the decoded [`TelemetrySample`] would bake today's
//! decoder bugs into the capture permanently. Storing the wire bytes means a
//! capture taken today can be re-decoded next year after a protocol fix, and
//! it doubles as a regression corpus for the decoders themselves.
//!
//! # Format (`.pwtl`)
//!
//! ```text
//! magic      "PWTL"        4 bytes
//! version    u32 LE        4 bytes
//! epoch_ns   u64 LE        8 bytes   wall-clock at capture start
//! ---- then, repeating: ----
//! t_ns       u64 LE        8 bytes   ns since capture start
//! len        u32 LE        4 bytes
//! payload    len bytes
//! ```

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"PWTL";
const VERSION: u32 = 1;
/// Refuse absurd frame lengths so a corrupt file can't make us allocate 4 GB.
const MAX_FRAME: u32 = 1 << 20;

pub struct Recorder {
    out: BufWriter<File>,
    frames: u64,
    bytes: u64,
}

impl Recorder {
    pub fn create(path: impl AsRef<Path>, epoch_ns: u64) -> io::Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&epoch_ns.to_le_bytes())?;
        Ok(Self { out, frames: 0, bytes: 0 })
    }

    /// Append one datagram. Called from the ingest thread, so it must not do
    /// anything that could block for long — `BufWriter` keeps this to a memcpy
    /// in the common case.
    pub fn write_frame(&mut self, t_ns: u64, payload: &[u8]) -> io::Result<()> {
        self.out.write_all(&t_ns.to_le_bytes())?;
        self.out.write_all(&(payload.len() as u32).to_le_bytes())?;
        self.out.write_all(payload)?;
        self.frames += 1;
        self.bytes += payload.len() as u64;
        Ok(())
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.out.flush()
    }
}

pub struct Frame {
    pub t_ns: u64,
    pub payload: Vec<u8>,
}

pub struct Replayer {
    frames: Vec<Frame>,
    cursor: usize,
}

impl Replayer {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut r = BufReader::new(File::open(path)?);

        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a .pwtl capture (bad magic)",
            ));
        }
        let mut u32buf = [0u8; 4];
        r.read_exact(&mut u32buf)?;
        let version = u32::from_le_bytes(u32buf);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported capture version {version}"),
            ));
        }
        let mut u64buf = [0u8; 8];
        r.read_exact(&mut u64buf)?; // epoch, informational

        let mut frames = Vec::new();
        loop {
            if r.read_exact(&mut u64buf).is_err() {
                break; // clean EOF
            }
            let t_ns = u64::from_le_bytes(u64buf);
            if r.read_exact(&mut u32buf).is_err() {
                break; // truncated tail — keep what we have
            }
            let len = u32::from_le_bytes(u32buf);
            if len > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame length {len} exceeds sanity limit"),
                ));
            }
            let mut payload = vec![0u8; len as usize];
            if r.read_exact(&mut payload).is_err() {
                break;
            }
            frames.push(Frame { t_ns, payload });
        }
        Ok(Self { frames, cursor: 0 })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn duration(&self) -> Duration {
        match (self.frames.first(), self.frames.last()) {
            (Some(a), Some(b)) => Duration::from_nanos(b.t_ns.saturating_sub(a.t_ns)),
            _ => Duration::ZERO,
        }
    }

    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    /// Capture timestamp of the next frame, without consuming it.
    ///
    /// Lets a caller driving its own clock (the UI, which must not block)
    /// drain exactly the frames that have come due this tick.
    pub fn peek_t_ns(&self) -> Option<u64> {
        self.frames.get(self.cursor).map(|f| f.t_ns)
    }

    /// Step exactly one frame, ignoring wall-clock timing. This is the
    /// frame-stepper for debugging a single corner.
    pub fn step(&mut self) -> Option<&Frame> {
        let f = self.frames.get(self.cursor)?;
        self.cursor += 1;
        Some(f)
    }

    /// Drive the capture in real time (or a multiple of it).
    ///
    /// `speed` of 1.0 replays at the original rate; 0.1 is slow motion for
    /// studying a braking zone; 10.0 races through a long stint.
    pub fn play<F>(&mut self, speed: f64, mut on_frame: F)
    where
        F: FnMut(&Frame),
    {
        let Some(first) = self.frames.first() else { return };
        let t0_capture = first.t_ns;
        let t0_wall = Instant::now();

        for f in &self.frames[self.cursor..] {
            let offset_ns = f.t_ns.saturating_sub(t0_capture) as f64 / speed.max(1e-6);
            let target = Duration::from_nanos(offset_ns as u64);
            loop {
                let elapsed = t0_wall.elapsed();
                if elapsed >= target {
                    break;
                }
                let remaining = target - elapsed;
                // Sleep coarsely, then spin the last 1 ms. `thread::sleep` on
                // a stock desktop kernel routinely overshoots by 1-15 ms,
                // which would smear the timing of a replayed lap.
                if remaining > Duration::from_millis(2) {
                    std::thread::sleep(remaining - Duration::from_millis(1));
                } else {
                    std::hint::spin_loop();
                }
            }
            on_frame(f);
        }
        self.cursor = self.frames.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pwtl_test_{name}_{}.pwtl", std::process::id()))
    }

    #[test]
    fn roundtrips_frames_byte_for_byte() {
        let p = tmp("roundtrip");
        let mut rec = Recorder::create(&p, 12345).unwrap();
        rec.write_frame(0, &[1, 2, 3]).unwrap();
        rec.write_frame(16_666_667, &[4, 5, 6, 7]).unwrap();
        assert_eq!(rec.frames(), 2);
        rec.finish().unwrap();

        let mut rp = Replayer::open(&p).unwrap();
        assert_eq!(rp.len(), 2);
        let f0 = rp.step().unwrap();
        assert_eq!(f0.t_ns, 0);
        assert_eq!(f0.payload, vec![1, 2, 3]);
        let f1 = rp.step().unwrap();
        assert_eq!(f1.t_ns, 16_666_667);
        assert_eq!(f1.payload, vec![4, 5, 6, 7]);
        assert!(rp.step().is_none());

        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rejects_a_file_that_is_not_a_capture() {
        let p = tmp("bogus");
        std::fs::write(&p, b"definitely not telemetry").unwrap();
        assert!(Replayer::open(&p).is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn truncated_capture_keeps_the_frames_it_has() {
        let p = tmp("truncated");
        let mut rec = Recorder::create(&p, 0).unwrap();
        rec.write_frame(0, &[9; 32]).unwrap();
        rec.write_frame(1000, &[8; 32]).unwrap();
        rec.finish().unwrap();

        // Chop the last frame's payload in half, as a crashed session would.
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 16);
        std::fs::write(&p, &bytes).unwrap();

        let rp = Replayer::open(&p).unwrap();
        assert_eq!(rp.len(), 1, "the intact frame survives");
        std::fs::remove_file(&p).ok();
    }
}
