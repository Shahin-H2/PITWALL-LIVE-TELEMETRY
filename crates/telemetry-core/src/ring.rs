//! Lock-free transport between the ingest thread and the render thread.
//!
//! Two structures, because gauges and traces want opposite things:
//!
//! * [`TripleBuffer`] — **latest-wins**, for the gauges. A tachometer needle
//!   does not care about the sample it missed 8 ms ago; it cares about *now*.
//!   Never blocks, never allocates, and the reader is guaranteed to see a
//!   complete sample (never a half-written one).
//!
//! * [`SpscRing`] — **lossy FIFO**, for the trace graphs and the analysis
//!   layer, which do need every sample in order.
//!
//! Neither ever takes a lock. A mutex here would let a 12 µs network-thread
//! stall become a dropped frame, which is precisely the jitter this whole
//! architecture exists to avoid.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const INDEX_MASK: usize = 0b11;
const FRESH_BIT: usize = 0b100;

/// Single-producer / single-consumer latest-value buffer.
///
/// Three slots: one the writer owns, one the reader owns, one in the middle
/// that they atomically swap. Because each side always owns a slot outright,
/// a write can never tear a read.
pub struct TripleBuffer<T: Copy> {
    slots: [UnsafeCell<T>; 3],
    shared: AtomicUsize,
}

// SAFETY: access to each slot is disjoint by construction — the producer only
// ever touches `Producer::idx`, the consumer only `Consumer::idx`, and the
// third index lives in `shared`. The atomic swap is the only synchronisation
// point and it uses AcqRel ordering, which publishes the slot write.
unsafe impl<T: Copy + Send> Send for TripleBuffer<T> {}
unsafe impl<T: Copy + Send> Sync for TripleBuffer<T> {}

pub struct Producer<T: Copy> {
    buf: Arc<TripleBuffer<T>>,
    idx: usize,
}

pub struct Consumer<T: Copy> {
    buf: Arc<TripleBuffer<T>>,
    idx: usize,
}

/// Create a connected producer/consumer pair.
pub fn triple_buffer<T: Copy + Default>() -> (Producer<T>, Consumer<T>) {
    let buf = Arc::new(TripleBuffer {
        slots: [
            UnsafeCell::new(T::default()),
            UnsafeCell::new(T::default()),
            UnsafeCell::new(T::default()),
        ],
        shared: AtomicUsize::new(2),
    });
    (
        Producer { buf: Arc::clone(&buf), idx: 0 },
        Consumer { buf, idx: 1 },
    )
}

impl<T: Copy> Producer<T> {
    /// Publish a value. Wait-free: one store plus one atomic swap.
    #[inline]
    pub fn publish(&mut self, value: T) {
        // SAFETY: `self.idx` is owned exclusively by this producer.
        unsafe { *self.buf.slots[self.idx].get() = value };
        let prev = self.buf.shared.swap(self.idx | FRESH_BIT, Ordering::AcqRel);
        self.idx = prev & INDEX_MASK;
    }
}

impl<T: Copy> Consumer<T> {
    /// Read the most recently published value.
    ///
    /// Returns the previous value again if nothing new arrived, so the caller
    /// can render at 240 Hz off a 60 Hz feed without special-casing.
    #[inline]
    pub fn latest(&mut self) -> T {
        if self.buf.shared.load(Ordering::Acquire) & FRESH_BIT != 0 {
            let prev = self.buf.shared.swap(self.idx, Ordering::AcqRel);
            self.idx = prev & INDEX_MASK;
        }
        // SAFETY: `self.idx` is owned exclusively by this consumer.
        unsafe { *self.buf.slots[self.idx].get() }
    }

    /// Whether a new value has landed since the last [`Self::latest`] call.
    #[inline]
    pub fn has_fresh(&self) -> bool {
        self.buf.shared.load(Ordering::Acquire) & FRESH_BIT != 0
    }
}

/// Bounded single-producer / single-consumer queue.
///
/// Capacity is rounded up to a power of two so the index wrap is a mask
/// instead of a division. When full, the producer **drops the newest sample
/// and counts it** rather than blocking — a stalled UI must never apply
/// backpressure to the network thread.
pub struct SpscRing<T: Copy> {
    slots: Box<[UnsafeCell<T>]>,
    mask: usize,
    head: AtomicUsize, // producer writes
    tail: AtomicUsize, // consumer reads
    dropped: AtomicUsize,
}

unsafe impl<T: Copy + Send> Send for SpscRing<T> {}
unsafe impl<T: Copy + Send> Sync for SpscRing<T> {}

impl<T: Copy + Default> SpscRing<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two().max(2);
        let mut v = Vec::with_capacity(cap);
        v.resize_with(cap, || UnsafeCell::new(T::default()));
        Self {
            slots: v.into_boxed_slice(),
            mask: cap - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }

    /// Producer side. Returns false when the ring was full and the value was
    /// dropped.
    #[inline]
    pub fn push(&self, value: T) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) > self.mask {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: this slot is beyond the consumer's tail, so it is ours.
        unsafe { *self.slots[head & self.mask].get() = value };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side.
    #[inline]
    pub fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: this slot is behind the producer's head, so it is ours.
        let v = unsafe { *self.slots[tail & self.mask].get() };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(v)
    }

    pub fn len(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn triple_buffer_returns_latest() {
        let (mut p, mut c) = triple_buffer::<u32>();
        assert_eq!(c.latest(), 0);
        p.publish(7);
        assert_eq!(c.latest(), 7);
        // No new publish: the previous value persists.
        assert_eq!(c.latest(), 7);
        p.publish(8);
        p.publish(9);
        assert_eq!(c.latest(), 9, "intermediate values are legitimately skipped");
    }

    #[test]
    fn triple_buffer_never_tears_under_contention() {
        // Each published value is a pair that must always agree. A torn read
        // would show mismatched halves.
        let (mut p, mut c) = triple_buffer::<(u64, u64)>();
        let writer = thread::spawn(move || {
            for i in 0..200_000u64 {
                p.publish((i, i));
            }
        });
        let mut reads = 0u64;
        for _ in 0..200_000 {
            let (a, b) = c.latest();
            assert_eq!(a, b, "torn read: {a} != {b}");
            reads += 1;
        }
        writer.join().unwrap();
        assert!(reads > 0);
    }

    #[test]
    fn spsc_ring_is_fifo_and_drops_when_full() {
        let r = SpscRing::<u32>::new(4);
        for i in 0..4 {
            assert!(r.push(i));
        }
        assert!(!r.push(99), "5th push into a 4-slot ring must be dropped");
        assert_eq!(r.dropped(), 1);
        for i in 0..4 {
            assert_eq!(r.pop(), Some(i));
        }
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn spsc_ring_survives_a_real_producer_thread() {
        let r = Arc::new(SpscRing::<u64>::new(1024));
        let rp = Arc::clone(&r);
        let producer = thread::spawn(move || {
            for i in 0..50_000u64 {
                while !rp.push(i) {
                    std::hint::spin_loop();
                }
            }
        });
        let mut expect = 0u64;
        while expect < 50_000 {
            if let Some(v) = r.pop() {
                assert_eq!(v, expect, "ordering violated");
                expect += 1;
            }
        }
        producer.join().unwrap();
    }
}
