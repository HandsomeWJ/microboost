pub mod echo;
pub mod noise_gate;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const RING_SIZE: usize = 48000 * 2;

/// Lock-free single-producer single-consumer ring buffer for audio.
/// Uses UnsafeCell + atomics — safe because only one thread writes and one reads.
pub struct SpscRing {
    buf: std::cell::UnsafeCell<Vec<f32>>,
    pub write: AtomicUsize,
    read: AtomicUsize,
}

unsafe impl Send for SpscRing {}
unsafe impl Sync for SpscRing {}

impl SpscRing {
    pub fn new(size: usize) -> Self {
        Self {
            buf: std::cell::UnsafeCell::new(vec![0.0; size]),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    pub fn reset(&self) {
        self.write.store(0, Ordering::Release);
        self.read.store(0, Ordering::Release);
    }

    /// Push a sample (called from input/producer thread only)
    #[inline]
    pub fn push(&self, sample: f32) {
        let w = self.write.load(Ordering::Relaxed);
        let buf = unsafe { &mut *self.buf.get() };
        buf[w % RING_SIZE] = sample;
        self.write.store((w + 1) % (RING_SIZE * 2), Ordering::Release);
    }

    /// Number of samples available to read
    #[inline]
    pub fn available(&self) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Relaxed);
        if w >= r { w - r } else { RING_SIZE * 2 - r + w }
    }

    /// Read sample at current position without advancing
    #[inline]
    pub fn peek(&self, offset: usize) -> f32 {
        let r = self.read.load(Ordering::Relaxed);
        let buf = unsafe { &*self.buf.get() };
        buf[(r + offset) % RING_SIZE]
    }

    /// Advance read pointer by n
    #[inline]
    pub fn advance(&self, n: usize) {
        let r = self.read.load(Ordering::Relaxed);
        self.read.store((r + n) % (RING_SIZE * 2), Ordering::Release);
    }

    /// Read a sample at an absolute position (for recording tap)
    #[inline]
    pub fn read_at(&self, pos: usize) -> f32 {
        let buf = unsafe { &*self.buf.get() };
        buf[pos % RING_SIZE]
    }
}

/// Single-producer ring addressed by absolute sample position, used for the
/// echo-cancellation reference. The producer only pushes; the consumer keeps
/// its own cursor (chosen from capture timestamps) and reads with [`get`](Self::get),
/// which returns `None` outside the window of samples still held.
pub struct RefRing {
    buf: std::cell::UnsafeCell<Vec<f32>>,
    write: AtomicU64,
    size: usize,
}

unsafe impl Send for RefRing {}
unsafe impl Sync for RefRing {}

impl RefRing {
    pub fn new(size: usize) -> Self {
        Self {
            buf: std::cell::UnsafeCell::new(vec![0.0; size]),
            write: AtomicU64::new(0),
            size,
        }
    }

    pub fn reset(&self) {
        self.write.store(0, Ordering::Release);
    }

    /// Producer thread only.
    #[inline]
    pub fn push(&self, sample: f32) {
        let w = self.write.load(Ordering::Relaxed);
        let buf = unsafe { &mut *self.buf.get() };
        buf[(w % self.size as u64) as usize] = sample;
        self.write.store(w + 1, Ordering::Release);
    }

    /// Absolute position the next pushed sample will get.
    #[inline]
    pub fn write_pos(&self) -> u64 {
        self.write.load(Ordering::Acquire)
    }

    /// Sample at absolute position `pos`, if it has been written and not yet
    /// overwritten.
    #[inline]
    pub fn get(&self, pos: u64) -> Option<f32> {
        let w = self.write_pos();
        if pos >= w || pos + (self.size as u64) < w {
            return None;
        }
        let buf = unsafe { &*self.buf.get() };
        Some(buf[(pos % self.size as u64) as usize])
    }
}

#[cfg(test)]
mod ref_ring_tests {
    use super::*;

    #[test]
    fn absolute_positions_and_window() {
        let r = RefRing::new(8);
        assert_eq!(r.get(0), None);
        for i in 0..10 {
            r.push(i as f32);
        }
        assert_eq!(r.write_pos(), 10);
        assert_eq!(r.get(9), Some(9.0));
        assert_eq!(r.get(2), Some(2.0)); // oldest still held: 10 - 8 = 2
        assert_eq!(r.get(1), None); // overwritten
        assert_eq!(r.get(10), None); // not yet written
    }
}
