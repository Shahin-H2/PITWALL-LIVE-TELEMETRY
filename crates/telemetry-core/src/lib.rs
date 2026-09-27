//! Live telemetry ingestion core.
//!
//! Decodes UDP telemetry from Forza, F1 25, and Assetto Corsa into a single
//! canonical [`sample::TelemetrySample`], and moves it to the renderer without
//! locks or allocation.
//!
//! ```no_run
//! use telemetry_core::{ingest, sample::FieldMask};
//!
//! let (handle, mut rx) = ingest::spawn(ingest::IngestConfig::default())?;
//! loop {
//!     let s = rx.latest();
//!     if s.has(FieldMask::GEAR) {
//!         println!("{} | gear {} | {:.0} km/h", handle.active_sim(), s.gear, s.speed_kmh());
//!     }
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

pub mod ingest;
pub mod protocols;
pub mod raw;
pub mod record;
pub mod registry;
pub mod ring;
pub mod sample;

pub use registry::{DetectionMode, Registry};
pub use sample::{FieldMask, SimId, TelemetrySample, Vec3, Wheel};
