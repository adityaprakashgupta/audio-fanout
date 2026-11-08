//! One "slave" output: pulls source-format audio from a ring buffer, keeps it
//! locked to the output device's clock by nudging a resampler ratio, converts
//! sample rate / channel count, and applies delay + gain.
//!
//! Pure logic (no audio API), so it is unit-testable and identical on every OS.

use anyhow::Result;
use ringbuf::traits::{Consumer, Observer};
use ringbuf::HeapCons;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;

const CHUNK: usize = 256; // source frames per resampler call
const MAX_REL: f64 = 1.01; // controller may move the ratio by at most +-1%

/// Readable from another thread for diagnostics.
#[derive(Default)]
pub struct Stats {
    pub fill_frames: AtomicU32,
    pub ratio_ppm: AtomicI32, // (ratio - 1) * 1e6
    pub target_frames: AtomicU32,
}

pub struct Slave {
    cons: HeapCons<f32>,
    src_ch: usize,
    out_ch: usize,
    src_rate: usize,
    target: usize, // source frames to keep buffered (= delay + cushion)
    resampler: SincFixedIn<f32>,
    inter: Vec<f32>,
    inbuf: Vec<Vec<f32>>,
    outbuf: Vec<Vec<f32>>,
    pending: VecDeque<f32>, // resampled, interleaved, out_ch
    primed: bool,
    err_avg: f64,
    gain: f32,
    stats: Arc<Stats>,
}

impl Slave {
    pub fn new(
        cons: HeapCons<f32>,
        src_ch: usize,
        src_rate: u32,
        out_ch: usize,
        out_rate: u32,
        delay_ms: u32,
        gain: f32,
        stats: Arc<Stats>,
    ) -> Result<Self> {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let base = out_rate as f64 / src_rate as f64;
        let resampler = SincFixedIn::<f32>::new(base, MAX_REL, params, CHUNK, src_ch)?;
        let outbuf = vec![vec![0.0f32; resampler.output_frames_max()]; src_ch];
        let src_rate = src_rate as usize;
        let target = delay_ms as usize * src_rate / 1000 + src_rate / 50; // delay + 20 ms
        stats.target_frames.store(target as u32, Ordering::Relaxed);
        Ok(Slave {
            cons,
            src_ch,
            out_ch,
            src_rate,
            target,
            resampler,
            inter: vec![0.0; CHUNK * src_ch],
            inbuf: vec![vec![0.0; CHUNK]; src_ch],
            outbuf,
            pending: VecDeque::with_capacity(out_rate as usize * out_ch / 5),
            primed: false,
            err_avg: 0.0,
            gain,
            stats,
        })
    }

    /// Current resample ratio relative to the nominal one (for tests/diagnostics).
    pub fn relative_ratio(&self) -> f64 {
        1.0 - (0.002 * self.err_avg).clamp(-0.005, 0.005)
    }

    /// Called from the audio callback. Never allocates (after construction).
    pub fn fill(&mut self, out: &mut [f32]) {
        let want = out.len();
        let (sc, oc) = (self.src_ch, self.out_ch);

        // Safety net only: if we are wildly behind, jump (this can click).
        let fill = self.cons.occupied_len() / sc;
        if fill > self.target + self.src_rate / 10 {
            self.cons.skip((fill - self.target) * sc);
        }
        // (Re)prime: wait until the delay is buffered before playing.
        if !self.primed && self.cons.occupied_len() / sc >= self.target {
            self.primed = true;
        }

        while self.primed && self.pending.len() < want && self.cons.occupied_len() >= CHUNK * sc {
            // Drift controller: ring too full => consume input faster (ratio < 1),
            // too empty => slower (ratio > 1). Smoothed so jitter doesn't modulate pitch.
            let fill = (self.cons.occupied_len() / sc) as f64;
            let err = (fill - self.target as f64) / self.target as f64;
            self.err_avg += 0.02 * (err - self.err_avg);
            let rel = self.relative_ratio();
            let _ = self.resampler.set_resample_ratio_relative(rel, true);
            self.stats.fill_frames.store(fill as u32, Ordering::Relaxed);
            self.stats
                .ratio_ppm
                .store(((rel - 1.0) * 1e6) as i32, Ordering::Relaxed);

            self.cons.pop_slice(&mut self.inter);
            for f in 0..CHUNK {
                for c in 0..sc {
                    self.inbuf[c][f] = self.inter[f * sc + c];
                }
            }
            let n = match self
                .resampler
                .process_into_buffer(&self.inbuf, &mut self.outbuf, None)
            {
                Ok((_, n)) => n,
                Err(_) => break,
            };
            for f in 0..n {
                if sc == oc {
                    for c in 0..oc {
                        self.pending.push_back(self.outbuf[c][f]);
                    }
                } else if oc == 1 {
                    let sum: f32 = (0..sc).map(|c| self.outbuf[c][f]).sum();
                    self.pending.push_back(sum / sc as f32);
                } else if sc == 1 {
                    for _ in 0..oc {
                        self.pending.push_back(self.outbuf[0][f]);
                    }
                } else {
                    // e.g. stereo -> 5.1: fill the channels we have, silence the rest
                    for c in 0..oc {
                        self.pending
                            .push_back(if c < sc { self.outbuf[c][f] } else { 0.0 });
                    }
                }
            }
        }

        let have = self.pending.len().min(want);
        for (o, s) in out[..have].iter_mut().zip(self.pending.drain(..have)) {
            *o = s * self.gain;
        }
        out[have..].fill(0.0);
        if have < want {
            self.primed = false; // underrun: rebuild the delay before playing again
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;

    fn sine_chunk(frames: usize, ch: usize, phase: &mut f32, step: f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * ch);
        for _ in 0..frames {
            let s = (*phase).sin() * 0.5;
            *phase += step;
            for _ in 0..ch {
                v.push(s);
            }
        }
        v
    }

    /// Device clock runs 300 ppm slow relative to the source: the controller must
    /// settle at ratio ~0.9997 while the ring stays near its target (no runaway).
    #[test]
    fn locks_to_slow_device_clock() {
        let (src_rate, ch) = (48_000u32, 2usize);
        let rb = HeapRb::<f32>::new(src_rate as usize * ch * 2);
        let (mut prod, cons) = rb.split();
        let stats = Arc::new(Stats::default());
        let mut slave =
            Slave::new(cons, ch, src_rate, ch, src_rate, 0, 1.0, stats.clone()).unwrap();

        let mut phase = 0.0f32;
        let mut out_acc = 0.0f64;
        let out_per_tick = 480.0 * (1.0 - 0.0003);
        for _ in 0..12_000 {
            // 10 ms tick: source delivers 480 frames, device consumes ~479.86
            prod.push_slice(&sine_chunk(480, ch, &mut phase, 0.05));
            out_acc += out_per_tick;
            let frames = out_acc as usize;
            out_acc -= frames as f64;
            let mut out = vec![0.0f32; frames * ch];
            slave.fill(&mut out);
        }
        let rel = slave.relative_ratio();
        let fill = stats.fill_frames.load(Ordering::Relaxed) as f64;
        let target = stats.target_frames.load(Ordering::Relaxed) as f64;
        assert!((rel - 0.9997).abs() < 0.0002, "ratio {rel}");
        assert!(
            fill > target * 0.5 && fill < target * 2.0,
            "fill {fill} target {target}"
        );
    }

    /// 48 kHz stereo source into a 44.1 kHz mono device: output must be audible
    /// and roughly the right length (no stalls).
    #[test]
    fn converts_rate_and_channels() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let mut slave = Slave::new(
            cons,
            2,
            48_000,
            1,
            44_100,
            0,
            1.0,
            Arc::new(Stats::default()),
        )
        .unwrap();
        let mut phase = 0.0f32;
        let (mut total, mut peak) = (0usize, 0.0f32);
        let mut acc = 0.0f64;
        for _ in 0..500 {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
            acc += 441.0;
            let frames = acc as usize;
            acc -= frames as f64;
            let mut out = vec![0.0f32; frames];
            slave.fill(&mut out);
            total += out.iter().filter(|s| **s != 0.0).count();
            peak = out.iter().fold(peak, |m, s| m.max(s.abs()));
        }
        assert!(peak > 0.4 && peak < 0.6, "peak {peak}");
        assert!(total > 500 * 441 * 9 / 10, "non-silent samples {total}");
    }

    #[test]
    fn delay_holds_back_start() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let mut slave = Slave::new(
            cons,
            2,
            48_000,
            2,
            48_000,
            100,
            1.0,
            Arc::new(Stats::default()),
        )
        .unwrap();
        let mut phase = 0.0f32;
        let mut first_audio_tick = None;
        for t in 0..40 {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
            let mut out = vec![0.0f32; 480 * 2];
            slave.fill(&mut out);
            if first_audio_tick.is_none() && out.iter().any(|s| *s != 0.0) {
                first_audio_tick = Some(t);
            }
        }
        // 100 ms delay + 20 ms cushion at 10 ms per tick => audio starts around tick 12.
        let t = first_audio_tick.expect("never started");
        assert!((11..=15).contains(&t), "started at tick {t}");
    }
}
