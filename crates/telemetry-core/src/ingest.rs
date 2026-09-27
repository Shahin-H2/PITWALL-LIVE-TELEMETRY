//! The UDP ingest thread.
//!
//! One thread, one socket, one job: get bytes off the wire, stamp them, decode
//! them, and hand them to the renderer without ever blocking it.
//!
//! Ordering inside the hot loop matters and is deliberate:
//!
//! 1. `recv_from` returns.
//! 2. **Timestamp immediately** — before decode, before anything. Every latency
//!    number the app reports is measured from this instant, so any work done
//!    ahead of it is work that silently vanishes from the measurement.
//! 3. Record the raw bytes (buffered; a memcpy in the common case).
//! 4. Decode.
//! 5. Publish.
//!
//! The thread never allocates after startup: the receive buffer is reused, the
//! sample is `Copy`, and both handoff structures are preallocated.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::protocols::assetto;
use crate::record::Recorder;
use crate::registry::Registry;
use crate::ring::{self, Consumer, Producer, SpscRing};
use crate::sample::{SimId, TelemetrySample};

/// Largest datagram we will accept. F1's biggest packet is ~1.5 KB; this is
/// generous and still far below the UDP fragmentation threshold.
const RECV_BUF: usize = 4096;

/// Live counters, published for the on-screen diagnostics overlay.
#[derive(Default)]
pub struct IngestStats {
    pub packets: AtomicU64,
    pub samples: AtomicU64,
    pub bytes: AtomicU64,
    pub decode_errors: AtomicU64,
    /// Slowest observed gap between consecutive packets, in microseconds.
    /// This is the honest measure of the *sim's* jitter, not ours.
    pub max_gap_us: AtomicU64,
    /// Time spent inside decode, in nanoseconds, for the last packet.
    pub last_decode_ns: AtomicU64,
}

pub struct IngestConfig {
    pub bind: SocketAddr,
    /// Where to send the Assetto Corsa handshake. `None` disables AC support.
    pub ac_server: Option<SocketAddr>,
    /// Path for a `.pwtl` capture of this session.
    pub record_to: Option<std::path::PathBuf>,
    /// Capacity of the history ring handed to the analysis layer.
    pub history_capacity: usize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:5000".parse().unwrap(),
            ac_server: None,
            record_to: None,
            history_capacity: 65536,
        }
    }
}

pub struct IngestHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub stats: Arc<IngestStats>,
    pub history: Arc<SpscRing<TelemetrySample>>,
    /// Which sim the registry settled on, as a `SimId as u8`.
    active_sim: Arc<AtomicU64>,
}

impl IngestHandle {
    pub fn active_sim(&self) -> SimId {
        match self.active_sim.load(Ordering::Relaxed) {
            1 => SimId::Forza,
            2 => SimId::F1_25,
            3 => SimId::AssettoCorsa,
            4 => SimId::Acc,
            _ => SimId::Unknown,
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for IngestHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start the ingest thread. Returns the handle plus the consumer end of the
/// latest-value buffer, which the renderer polls once per frame.
pub fn spawn(cfg: IngestConfig) -> std::io::Result<(IngestHandle, Consumer<TelemetrySample>)> {
    let socket = UdpSocket::bind(cfg.bind)?;
    // A read timeout is what lets the thread notice the stop flag when no
    // packets are arriving — otherwise `recv_from` parks forever and the app
    // hangs on exit.
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;

    let (producer, consumer) = ring::triple_buffer::<TelemetrySample>();
    let stats = Arc::new(IngestStats::default());
    let history = Arc::new(SpscRing::<TelemetrySample>::new(cfg.history_capacity));
    let stop = Arc::new(AtomicBool::new(false));
    let active_sim = Arc::new(AtomicU64::new(0));

    let thread = {
        let stats = Arc::clone(&stats);
        let history = Arc::clone(&history);
        let stop = Arc::clone(&stop);
        let active_sim = Arc::clone(&active_sim);
        thread::Builder::new()
            .name("telemetry-ingest".into())
            .spawn(move || {
                ingest_loop(socket, cfg, producer, stats, history, stop, active_sim);
            })?
    };

    Ok((
        IngestHandle {
            stop,
            thread: Some(thread),
            stats,
            history,
            active_sim,
        },
        consumer,
    ))
}

fn ingest_loop(
    socket: UdpSocket,
    cfg: IngestConfig,
    mut producer: Producer<TelemetrySample>,
    stats: Arc<IngestStats>,
    history: Arc<SpscRing<TelemetrySample>>,
    stop: Arc<AtomicBool>,
    active_sim: Arc<AtomicU64>,
) {
    let epoch = Instant::now();
    let epoch_wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let mut recorder = cfg.record_to.as_ref().and_then(|p| {
        match Recorder::create(p, epoch_wall) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("[ingest] could not open capture file: {e}");
                None
            }
        }
    });

    let mut registry = Registry::new();
    let mut buf = [0u8; RECV_BUF];
    let mut last_packet: Option<Instant> = None;
    let mut ac_handshake_sent_at: Option<Instant> = None;

    // Assetto Corsa is request/response: nothing arrives until we ask.
    if let Some(ac) = cfg.ac_server {
        if socket.send_to(&assetto::handshake_packet(), ac).is_ok() {
            ac_handshake_sent_at = Some(Instant::now());
        }
    }

    while !stop.load(Ordering::Relaxed) {
        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Idle tick. Retry the AC handshake — the sim may have started
                // after we did.
                if let (Some(ac), Some(sent)) = (cfg.ac_server, ac_handshake_sent_at) {
                    if sent.elapsed() > Duration::from_secs(2)
                        && registry.active() == SimId::Unknown
                    {
                        let _ = socket.send_to(&assetto::handshake_packet(), ac);
                        ac_handshake_sent_at = Some(Instant::now());
                    }
                }
                continue;
            }
            Err(e) => {
                eprintln!("[ingest] recv error: {e}");
                continue;
            }
        };

        // (2) Stamp first. Everything downstream measures from here.
        let now = Instant::now();
        let t_capture_ns = now.duration_since(epoch).as_nanos() as u64;

        if let Some(prev) = last_packet {
            let gap_us = now.duration_since(prev).as_micros() as u64;
            stats.max_gap_us.fetch_max(gap_us, Ordering::Relaxed);
        }
        last_packet = Some(now);

        stats.packets.fetch_add(1, Ordering::Relaxed);
        stats.bytes.fetch_add(len as u64, Ordering::Relaxed);

        let payload = &buf[..len];

        // (3) Record raw bytes before decoding, so a capture is never polluted
        // by decoder behaviour.
        if let Some(rec) = recorder.as_mut() {
            if let Err(e) = rec.write_frame(t_capture_ns, payload) {
                eprintln!("[ingest] capture write failed, recording stopped: {e}");
                recorder = None;
            }
        }

        // AC's handshake response means "now ask me for updates".
        if assetto::is_handshake_response(payload) {
            let _ = socket.send_to(&assetto::subscribe_packet(), from);
            registry.pin(SimId::AssettoCorsa);
            registry.unpin();
            active_sim.store(SimId::AssettoCorsa as u64, Ordering::Relaxed);
            continue;
        }

        // (4) Decode.
        let decode_start = Instant::now();
        let decoded = registry.ingest(payload, t_capture_ns);
        stats
            .last_decode_ns
            .store(decode_start.elapsed().as_nanos() as u64, Ordering::Relaxed);

        active_sim.store(registry.active() as u64, Ordering::Relaxed);

        // (5) Publish.
        match decoded {
            Some(sample) => {
                stats.samples.fetch_add(1, Ordering::Relaxed);
                producer.publish(sample);
                history.push(sample);
            }
            None => {
                stats.decode_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    if let (Some(ac), true) = (cfg.ac_server, registry.active() == SimId::AssettoCorsa) {
        let _ = socket.send_to(&assetto::dismiss_packet(), ac);
    }
    if let Some(rec) = recorder {
        let frames = rec.frames();
        if let Err(e) = rec.finish() {
            eprintln!("[ingest] capture flush failed: {e}");
        } else {
            eprintln!("[ingest] capture closed after {frames} frames");
        }
    }
}
