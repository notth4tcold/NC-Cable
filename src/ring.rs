//! Lock-free single-producer / single-consumer ring buffer of interleaved f32 frames.
//!
//! This is the heart of the cable, the Linux counterpart of the circular buffer the
//! VB-CABLE driver allocates at startup (`max_latency + 8` frames, 32-bit samples):
//! the render side (cable input / sink) pushes frames, the capture side
//! (cable output / source) pulls them after a configurable latency.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const MAX_CHANNELS: usize = 16;

/// Counters shown by `nc-cable status`, modelled after the VB-CABLE control panel
/// ("Push loss", "Pull loss", buffer-size histogram, latency warnings, levels).
#[derive(Default)]
pub struct Stats {
    pub push_calls: AtomicU64,
    pub pull_calls: AtomicU64,
    /// Frames dropped because the ring was full (writer faster than reader).
    pub push_loss: AtomicU64,
    /// Frames replaced by silence because the ring was empty (reader faster than writer).
    pub pull_loss: AtomicU64,
    pub overflows: AtomicU64,
    pub underruns: AtomicU64,
    /// Times the reader skipped ahead to bring the fill level back to the target.
    pub resyncs: AtomicU64,
    /// Writer buffer sizes: < 256, < 512, < 1024, >= 1024 frames.
    pub quantum_hist: [AtomicU64; 4],
    /// Writer buffers whose size x3 exceeds the target latency (same rule as the driver).
    pub latency_warnings: AtomicU64,
    pub last_quantum_in: AtomicU32,
    pub last_quantum_out: AtomicU32,
    /// Peak absolute sample per channel since the last read, stored as f32 bits.
    /// For non-negative floats the bit pattern orders like the value, so `fetch_max` works.
    pub peak_in: [AtomicU32; MAX_CHANNELS],
    pub peak_out: [AtomicU32; MAX_CHANNELS],
}

pub struct Ring {
    buf: Box<[UnsafeCell<f32>]>,
    channels: usize,
    capacity: usize,
    /// Monotonic frame counters; only the producer stores `write`, only the consumer `read`.
    write: AtomicUsize,
    read: AtomicUsize,
    target: AtomicUsize,
    pub stats: Stats,
}

// SAFETY: a slot is written by the producer only while it is outside [read, write),
// and read by the consumer only while it is inside; the Release/Acquire pairs on the
// counters order those accesses.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new(channels: usize, capacity: usize, target: usize) -> Arc<Self> {
        assert!((1..=MAX_CHANNELS).contains(&channels));
        assert!(capacity > 0);
        let buf = (0..channels * capacity)
            .map(|_| UnsafeCell::new(0.0))
            .collect();
        Arc::new(Ring {
            buf,
            channels,
            capacity,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            target: AtomicUsize::new(target.min(capacity)),
            stats: Stats::default(),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn target(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    pub fn fill(&self) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    fn slot(&self, frame: usize) -> *mut f32 {
        self.buf[(frame % self.capacity) * self.channels].get()
    }

    /// Copies `frames` frames starting at ring position `pos` into or out of `data`,
    /// splitting at the wrap point.
    unsafe fn copy(&self, pos: usize, frames: usize, mut f: impl FnMut(*mut f32, usize, usize)) {
        let start = pos % self.capacity;
        let first = frames.min(self.capacity - start);
        f(self.slot(pos), 0, first * self.channels);
        if first < frames {
            f(
                self.slot(pos + first),
                first * self.channels,
                (frames - first) * self.channels,
            );
        }
    }
}

fn track_peaks(peaks: &[AtomicU32; MAX_CHANNELS], data: &[f32], channels: usize) {
    let mut local = [0f32; MAX_CHANNELS];
    for frame in data.chunks_exact(channels) {
        for (p, s) in local.iter_mut().zip(frame) {
            *p = p.max(s.abs());
        }
    }
    for (peak, value) in peaks.iter().zip(&local[..channels]) {
        peak.fetch_max(value.to_bits(), Ordering::Relaxed);
    }
}

pub struct Producer {
    ring: Arc<Ring>,
}

pub struct Consumer {
    ring: Arc<Ring>,
    primed: bool,
    /// Writer call count seen at the previous pull, to tell a stalled writer
    /// (real loss) from one that stopped because playback ended.
    last_push_calls: u64,
}

pub fn split(ring: &Arc<Ring>) -> (Producer, Consumer) {
    (
        Producer { ring: ring.clone() },
        Consumer {
            ring: ring.clone(),
            primed: false,
            last_push_calls: 0,
        },
    )
}

impl Producer {
    /// Pushes interleaved frames, dropping what does not fit. Returns frames written.
    pub fn push(&mut self, data: &[f32]) -> usize {
        let ring = &*self.ring;
        let ch = ring.channels;
        let frames = data.len() / ch;
        let stats = &ring.stats;

        stats.push_calls.fetch_add(1, Ordering::Relaxed);
        stats
            .last_quantum_in
            .store(frames as u32, Ordering::Relaxed);
        let bucket = match frames {
            0..256 => 0,
            256..512 => 1,
            512..1024 => 2,
            _ => 3,
        };
        stats.quantum_hist[bucket].fetch_add(1, Ordering::Relaxed);
        if frames * 3 > ring.target() {
            stats.latency_warnings.fetch_add(1, Ordering::Relaxed);
        }
        track_peaks(&stats.peak_in, &data[..frames * ch], ch);

        let w = ring.write.load(Ordering::Relaxed);
        let r = ring.read.load(Ordering::Acquire);
        let free = ring.capacity - w.wrapping_sub(r);
        let n = frames.min(free);
        // SAFETY: frames [w, w + n) are outside [r, w) and therefore owned by the producer.
        unsafe {
            ring.copy(w, n, |dst, off, len| {
                std::ptr::copy_nonoverlapping(data[off..off + len].as_ptr(), dst, len)
            });
        }
        ring.write.store(w.wrapping_add(n), Ordering::Release);

        if n < frames {
            stats.overflows.fetch_add(1, Ordering::Relaxed);
            stats
                .push_loss
                .fetch_add((frames - n) as u64, Ordering::Relaxed);
        }
        n
    }
}

impl Consumer {
    /// Fills `out` with interleaved frames, using silence while the ring is priming
    /// up to the target latency or when it runs dry.
    pub fn pull(&mut self, out: &mut [f32]) {
        let ring = &*self.ring;
        let ch = ring.channels;
        let frames = out.len() / ch;
        let out = &mut out[..frames * ch];
        let stats = &ring.stats;
        let target = ring.target();

        stats.pull_calls.fetch_add(1, Ordering::Relaxed);
        stats
            .last_quantum_out
            .store(frames as u32, Ordering::Relaxed);

        let push_calls = stats.push_calls.load(Ordering::Relaxed);
        let writer_active = push_calls != self.last_push_calls;
        self.last_push_calls = push_calls;

        let mut r = ring.read.load(Ordering::Relaxed);
        let w = ring.write.load(Ordering::Acquire);
        let mut fill = w.wrapping_sub(r);

        if !self.primed {
            // While priming, hold data back until the target is reached, unless the
            // writer has stopped: then flush the tail instead of leaving it stuck
            // in front of the next stream.
            if fill < target.max(frames) && (writer_active || fill == 0) {
                out.fill(0.0);
                return;
            }
            self.primed = true;
        }

        // Clock drift / bursty writer: never let latency grow past twice the target.
        let high_water = 2 * target + frames;
        if fill > high_water {
            let skip = fill - (target + frames);
            r = r.wrapping_add(skip);
            fill -= skip;
            stats.resyncs.fetch_add(1, Ordering::Relaxed);
        }

        let n = frames.min(fill);
        // SAFETY: frames [r, r + n) are inside [read, write) and owned by the consumer.
        unsafe {
            ring.copy(r, n, |src, off, len| {
                std::ptr::copy_nonoverlapping(src, out[off..off + len].as_mut_ptr(), len)
            });
        }
        ring.read.store(r.wrapping_add(n), Ordering::Release);

        if n < frames {
            out[n * ch..].fill(0.0);
            if writer_active {
                stats.underruns.fetch_add(1, Ordering::Relaxed);
                stats
                    .pull_loss
                    .fetch_add((frames - n) as u64, Ordering::Relaxed);
            }
            self.primed = false;
        }
        track_peaks(&stats.peak_out, out, ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: usize, frames: usize, ch: usize) -> Vec<f32> {
        (start * ch..(start + frames) * ch)
            .map(|i| i as f32)
            .collect()
    }

    #[test]
    fn primes_then_passes_data_through_in_order() {
        let ring = Ring::new(2, 64, 8);
        let (mut p, mut c) = split(&ring);
        let mut out = vec![0.0; 2 * 4];

        p.push(&ramp(0, 4, 2));
        c.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "still priming below target");

        p.push(&ramp(4, 4, 2));
        c.pull(&mut out);
        assert_eq!(out, ramp(0, 4, 2));
        c.pull(&mut out);
        assert_eq!(out, ramp(4, 4, 2));
        assert_eq!(ring.stats.pull_loss.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn wraps_around_capacity() {
        let ring = Ring::new(1, 10, 1);
        let (mut p, mut c) = split(&ring);
        let mut out = vec![0.0; 7];
        for round in 0..5 {
            p.push(&ramp(round * 7, 7, 1));
            c.pull(&mut out);
            assert_eq!(out, ramp(round * 7, 7, 1));
        }
    }

    #[test]
    fn overflow_drops_and_counts_push_loss() {
        let ring = Ring::new(1, 8, 4);
        let (mut p, _c) = split(&ring);
        assert_eq!(p.push(&ramp(0, 6, 1)), 6);
        assert_eq!(p.push(&ramp(6, 6, 1)), 2);
        assert_eq!(ring.stats.push_loss.load(Ordering::Relaxed), 4);
        assert_eq!(ring.stats.overflows.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn underrun_pads_silence_and_reprimes() {
        let ring = Ring::new(1, 32, 4);
        let (mut p, mut c) = split(&ring);
        let mut out = vec![0.0; 4];
        p.push(&[1.0; 6]);
        c.pull(&mut out);
        assert_eq!(out, [1.0; 4]);
        p.push(&[]); // writer still running, just starved
        c.pull(&mut out);
        assert_eq!(out, [1.0, 1.0, 0.0, 0.0]);
        assert_eq!(ring.stats.pull_loss.load(Ordering::Relaxed), 2);
        p.push(&[2.0; 2]);
        c.pull(&mut out);
        assert_eq!(out, [0.0; 4], "re-priming after underrun");
    }

    #[test]
    fn drained_after_writer_stops_is_not_a_loss() {
        let ring = Ring::new(1, 32, 4);
        let (mut p, mut c) = split(&ring);
        let mut out = vec![0.0; 4];
        p.push(&[1.0; 6]);
        c.pull(&mut out);
        c.pull(&mut out);
        assert_eq!(out, [1.0, 1.0, 0.0, 0.0], "tail still delivered");
        c.pull(&mut out);
        assert_eq!(ring.stats.pull_loss.load(Ordering::Relaxed), 0);
        assert_eq!(ring.stats.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn tail_left_while_priming_is_flushed_when_writer_stops() {
        let ring = Ring::new(1, 32, 8);
        let (mut p, mut c) = split(&ring);
        let mut out = vec![0.0; 4];
        p.push(&[3.0; 3]);
        c.pull(&mut out);
        assert_eq!(out, [0.0; 4], "writer active: keep priming");
        c.pull(&mut out);
        assert_eq!(out, [3.0, 3.0, 3.0, 0.0], "writer stopped: tail flushed");
        assert_eq!(ring.fill(), 0);
        assert_eq!(ring.stats.pull_loss.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn resync_bounds_latency() {
        let ring = Ring::new(1, 128, 4);
        let (mut p, mut c) = split(&ring);
        let mut out = vec![0.0; 4];
        p.push(&ramp(0, 40, 1));
        c.pull(&mut out);
        assert_eq!(ring.stats.resyncs.load(Ordering::Relaxed), 1);
        assert_eq!(ring.fill(), 4, "fill brought back to target");
        assert_eq!(out, ramp(32, 4, 1));
    }

    #[test]
    fn tracks_peaks_and_latency_warning() {
        let ring = Ring::new(2, 64, 6);
        let (mut p, _c) = split(&ring);
        p.push(&[0.5, -0.25, -0.75, 0.1]);
        assert_eq!(
            f32::from_bits(ring.stats.peak_in[0].load(Ordering::Relaxed)),
            0.75
        );
        assert_eq!(
            f32::from_bits(ring.stats.peak_in[1].load(Ordering::Relaxed)),
            0.25
        );
        p.push(&[0.0; 2 * 3]);
        assert_eq!(ring.stats.latency_warnings.load(Ordering::Relaxed), 1);
    }
}
