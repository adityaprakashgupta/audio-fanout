//! One "slave" output: pulls source-format audio from a ring buffer, keeps it
//! locked to the output device's clock by nudging a resampler ratio, converts
//! sample rate / channel count, and applies delay + gain.
//!
//! Delay and volume are live settings ([`Settings`]) that the UI can change at any
//! time. Pure logic (no audio API), so it is unit-testable and identical on every OS.

use anyhow::Result;
use ringbuf::traits::{Consumer, Observer};
use ringbuf::HeapCons;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;

const CHUNK: usize = 256; // source frames per resampler call
const MAX_REL: f64 = 1.01; // controller may move the ratio by at most +-1%
/// Ring buffers hold 2 s, so the delay (plus cushion and jitter) must stay below that.
pub const MAX_DELAY_MS: u32 = 1500;

/// Readable from another thread for diagnostics.
#[derive(Default)]
pub struct Stats {
    pub fill_frames: AtomicU32,
    pub ratio_ppm: AtomicI32, // (ratio - 1) * 1e6
    pub target_frames: AtomicU32,
    pub connected: AtomicBool, // set by the supervisor
    pub reconnects: AtomicU32, // times an established connection was lost
}

/// Per-output settings, changeable at any time from the UI thread.
pub struct Settings {
    pub delay_ms: AtomicU32,
    pub volume_pct: AtomicU32, // 0..=100
    pub muted: AtomicBool,
}

impl Settings {
    pub fn new(delay_ms: u32, volume_pct: u32) -> Self {
        Settings {
            delay_ms: AtomicU32::new(delay_ms.min(MAX_DELAY_MS)),
            volume_pct: AtomicU32::new(volume_pct.min(100)),
            muted: AtomicBool::new(false),
        }
    }

    /// Returns the new value.
    pub fn adjust_delay(&self, delta_ms: i32) -> u32 {
        let v = (self.delay_ms.load(Ordering::Relaxed) as i64 + delta_ms as i64)
            .clamp(0, MAX_DELAY_MS as i64) as u32;
        self.delay_ms.store(v, Ordering::Relaxed);
        v
    }

    /// Returns the new value.
    pub fn adjust_volume(&self, delta_pct: i32) -> u32 {
        let v = (self.volume_pct.load(Ordering::Relaxed) as i64 + delta_pct as i64).clamp(0, 100) as u32;
        self.volume_pct.store(v, Ordering::Relaxed);
        v
    }

    /// Returns the new muted state.
    pub fn toggle_mute(&self) -> bool {
        !self.muted.fetch_xor(true, Ordering::Relaxed)
    }

    pub fn gain(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            self.volume_pct.load(Ordering::Relaxed) as f32 / 100.0
        }
    }
}

pub struct Slave {
    cons: HeapCons<f32>,
    src_ch: usize,
    out_ch: usize,
    src_rate: usize,
    target: usize, // source frames to keep buffered (= delay + cushion)
    applied_delay: u32,
    resampler: SincFixedIn<f32>,
    inter: Vec<f32>,
    inbuf: Vec<Vec<f32>>,
    outbuf: Vec<Vec<f32>>,
    pending: VecDeque<f32>, // resampled, interleaved, out_ch
    primed: bool,
    err_avg: f64,
    settings: Arc<Settings>,
    master: Arc<AtomicU32>, // OS volume as f32 bits (see volume.rs)
    cur_gain: f32,          // ramped toward master * volume to avoid clicks
    stats: Arc<Stats>,
}

fn make_resampler(out_rate: u32, src_rate: u32, src_ch: usize) -> Result<SincFixedIn<f32>> {
    let params = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let base = out_rate as f64 / src_rate as f64;
    Ok(SincFixedIn::<f32>::new(base, MAX_REL, params, CHUNK, src_ch)?)
}

fn target_frames(delay_ms: u32, src_rate: usize) -> usize {
    delay_ms as usize * src_rate / 1000 + src_rate / 50 // delay + 20 ms cushion
}

impl Slave {
    /// Starts with the output format equal to the source format. Call
    /// [`Slave::reconfigure`] once the real device (and its format) is known.
    pub fn new(
        cons: HeapCons<f32>,
        src_ch: usize,
        src_rate: u32,
        settings: Arc<Settings>,
        master: Arc<AtomicU32>,
        stats: Arc<Stats>,
    ) -> Result<Self> {
        let resampler = make_resampler(src_rate, src_rate, src_ch)?;
        let outbuf = vec![vec![0.0f32; resampler.output_frames_max()]; src_ch];
        let src_rate = src_rate as usize;
        let applied_delay = settings.delay_ms.load(Ordering::Relaxed);
        let target = target_frames(applied_delay, src_rate);
        stats.target_frames.store(target as u32, Ordering::Relaxed);
        Ok(Slave {
            cons,
            src_ch,
            out_ch: src_ch,
            src_rate,
            target,
            applied_delay,
            resampler,
            inter: vec![0.0; CHUNK * src_ch],
            inbuf: vec![vec![0.0; CHUNK]; src_ch],
            outbuf,
            pending: VecDeque::with_capacity(src_rate * src_ch / 5),
            primed: false,
            err_avg: 0.0,
            cur_gain: f32::from_bits(master.load(Ordering::Relaxed)) * settings.gain(),
            settings,
            master,
            stats,
        })
    }

    /// Throw away everything buffered and start over: stale audio is flushed, the
    /// drift controller is reset and the delay is rebuilt from live audio.
    /// Call while no audio callback is running (the supervisor guarantees this).
    pub fn reset(&mut self) {
        let n = self.cons.occupied_len();
        self.cons.skip(n);
        self.pending.clear();
        self.primed = false;
        self.err_avg = 0.0;
        self.cur_gain = f32::from_bits(self.master.load(Ordering::Relaxed)) * self.settings.gain();
        self.resampler.reset();
        let _ = self.resampler.set_resample_ratio_relative(1.0, false);
    }

    /// Adapt to a (re)connected device, which may run at a different rate or channel
    /// count than last time (e.g. a Bluetooth codec change). Implies [`Slave::reset`].
    pub fn reconfigure(&mut self, out_ch: usize, out_rate: u32) -> Result<()> {
        self.resampler = make_resampler(out_rate, self.src_rate as u32, self.src_ch)?;
        self.outbuf = vec![vec![0.0f32; self.resampler.output_frames_max()]; self.src_ch];
        self.out_ch = out_ch;
        self.pending = VecDeque::with_capacity(out_rate as usize * out_ch / 5);
        self.reset();
        Ok(())
    }

    /// The user changed the delay while playing.
    ///  * longer: wait for more audio to build up (a short silence of the difference)
    ///  * shorter: drop the surplus audio and fade back in
    fn apply_delay(&mut self, delay_ms: u32) {
        let old = self.target;
        self.applied_delay = delay_ms;
        self.target = target_frames(delay_ms, self.src_rate);
        self.stats.target_frames.store(self.target as u32, Ordering::Relaxed);
        self.err_avg = 0.0;
        if self.target > old {
            self.primed = false;
        } else if self.target < old {
            let surplus = (old - self.target) * self.src_ch;
            let n = surplus.min(self.cons.occupied_len());
            self.cons.skip(n);
            self.cur_gain = 0.0; // fade in across the jump instead of clicking
        }
    }

    /// Current resample ratio relative to the nominal one (for tests/diagnostics).
    pub fn relative_ratio(&self) -> f64 {
        1.0 - (0.002 * self.err_avg).clamp(-0.005, 0.005)
    }

    /// Called from the audio callback. Never allocates (after construction).
    pub fn fill(&mut self, out: &mut [f32]) {
        let want = out.len();
        let (sc, oc) = (self.src_ch, self.out_ch);

        let d = self.settings.delay_ms.load(Ordering::Relaxed);
        if d != self.applied_delay {
            self.apply_delay(d);
        }

        // Safety net only: if we are wildly behind, jump (this can click).
        let fill = self.cons.occupied_len() / sc;
        if fill > self.target + self.src_rate / 10 {
            self.cons.skip((fill - self.target) * sc);
        }
        // (Re)prime: wait until the delay is buffered before playing, then fade in.
        if !self.primed && self.cons.occupied_len() / sc >= self.target {
            self.primed = true;
            self.cur_gain = 0.0;
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
            self.stats.ratio_ppm.store(((rel - 1.0) * 1e6) as i32, Ordering::Relaxed);

            self.cons.pop_slice(&mut self.inter);
            for f in 0..CHUNK {
                for c in 0..sc {
                    self.inbuf[c][f] = self.inter[f * sc + c];
                }
            }
            let n = match self.resampler.process_into_buffer(&self.inbuf, &mut self.outbuf, None) {
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
                        self.pending.push_back(if c < sc { self.outbuf[c][f] } else { 0.0 });
                    }
                }
            }
        }

        let have = self.pending.len().min(want);
        // Output volume x OS volume, ramped linearly across this buffer (no zipper clicks).
        let target_gain = f32::from_bits(self.master.load(Ordering::Relaxed)) * self.settings.gain();
        let frames = (have / oc).max(1);
        let step = (target_gain - self.cur_gain) / frames as f32;
        let mut src = self.pending.drain(..have);
        for frame in out[..have].chunks_exact_mut(oc) {
            self.cur_gain += step;
            for o in frame.iter_mut() {
                *o = src.next().unwrap_or(0.0) * self.cur_gain;
            }
        }
        drop(src);
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

    fn unity() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(1.0f32.to_bits()))
    }

    fn settings(delay_ms: u32, vol: u32) -> Arc<Settings> {
        Arc::new(Settings::new(delay_ms, vol))
    }

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

    fn peak(out: &[f32]) -> f32 {
        out.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    /// Device clock runs 300 ppm slow relative to the source: the controller must
    /// settle at ratio ~0.9997 while the ring stays near its target (no runaway).
    #[test]
    fn locks_to_slow_device_clock() {
        let (src_rate, ch) = (48_000u32, 2usize);
        let rb = HeapRb::<f32>::new(src_rate as usize * ch * 2);
        let (mut prod, cons) = rb.split();
        let stats = Arc::new(Stats::default());
        let mut slave = Slave::new(cons, ch, src_rate, settings(0, 100), unity(), stats.clone()).unwrap();

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
        assert!(fill > target * 0.5 && fill < target * 2.0, "fill {fill} target {target}");
    }

    /// 48 kHz stereo source into a 44.1 kHz mono device: output must be audible
    /// and roughly the right length (no stalls).
    #[test]
    fn converts_rate_and_channels() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let mut slave =
            Slave::new(cons, 2, 48_000, settings(0, 100), unity(), Arc::new(Stats::default())).unwrap();
        slave.reconfigure(1, 44_100).unwrap();
        let mut phase = 0.0f32;
        let (mut total, mut pk) = (0usize, 0.0f32);
        let mut acc = 0.0f64;
        for _ in 0..500 {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
            acc += 441.0;
            let frames = acc as usize;
            acc -= frames as f64;
            let mut out = vec![0.0f32; frames];
            slave.fill(&mut out);
            total += out.iter().filter(|s| **s != 0.0).count();
            pk = pk.max(peak(&out));
        }
        assert!(pk > 0.4 && pk < 0.6, "peak {pk}");
        assert!(total > 500 * 441 * 9 / 10, "non-silent samples {total}");
    }

    #[test]
    fn delay_holds_back_start() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let mut slave =
            Slave::new(cons, 2, 48_000, settings(100, 100), unity(), Arc::new(Stats::default())).unwrap();
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

    /// OS volume changes must scale the output smoothly, and mute must reach silence.
    #[test]
    fn follows_master_volume_and_mute() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let master = unity();
        let mut slave =
            Slave::new(cons, 2, 48_000, settings(0, 100), master.clone(), Arc::new(Stats::default()))
                .unwrap();
        let mut phase = 0.0f32;
        let mut run = |ticks: usize, slave: &mut Slave| -> f32 {
            let mut pk = 0.0f32;
            for _ in 0..ticks {
                prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
                let mut out = vec![0.0f32; 480 * 2];
                slave.fill(&mut out);
                pk = peak(&out);
            }
            pk
        };
        let full = run(60, &mut slave);
        assert!(full > 0.45, "full {full}");
        master.store(0.25f32.to_bits(), Ordering::Relaxed);
        let quarter = run(60, &mut slave);
        assert!((quarter - full * 0.25).abs() < 0.03, "quarter {quarter} vs {full}");
        master.store(0.0f32.to_bits(), Ordering::Relaxed);
        let muted = run(60, &mut slave);
        assert!(muted < 0.001, "muted {muted}");
    }

    /// A reconnect must flush stale audio (so the device does not play seconds-old
    /// sound) and rebuild the delay from live audio, like a fresh start.
    #[test]
    fn reset_flushes_stale_audio_and_reprimes() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let mut slave =
            Slave::new(cons, 2, 48_000, settings(0, 100), unity(), Arc::new(Stats::default())).unwrap();
        let mut phase = 0.0f32;
        // Device was away: a full second of old audio piled up in the ring.
        for _ in 0..100 {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
        }
        slave.reset();
        // Nothing stale may come out, and it must stay silent until re-primed.
        let mut out = vec![0.0f32; 480 * 2];
        slave.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0.0), "stale audio leaked");
        // Live audio resumes after the cushion (20 ms) is rebuilt.
        let mut started = None;
        for t in 0..10 {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
            let mut out = vec![0.0f32; 480 * 2];
            slave.fill(&mut out);
            if started.is_none() && out.iter().any(|s| *s != 0.0) {
                started = Some(t);
            }
        }
        assert!(matches!(started, Some(t) if t <= 4), "restart {started:?}");
    }

    /// Reconnecting to a device with a different format must keep audio flowing.
    #[test]
    fn reconfigure_to_new_format_keeps_audio_flowing() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let mut slave =
            Slave::new(cons, 2, 48_000, settings(0, 100), unity(), Arc::new(Stats::default())).unwrap();
        let mut phase = 0.0f32;
        for (ch, rate) in [(2usize, 48_000u32), (1, 44_100), (2, 96_000)] {
            slave.reconfigure(ch, rate).unwrap();
            let per_tick = rate as f64 / 100.0;
            let (mut acc, mut pk) = (0.0f64, 0.0f32);
            for _ in 0..200 {
                prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
                acc += per_tick;
                let frames = acc as usize;
                acc -= frames as f64;
                let mut out = vec![0.0f32; frames * ch];
                slave.fill(&mut out);
                pk = pk.max(peak(&out));
            }
            assert!(pk > 0.4, "{ch} ch @ {rate}: peak {pk}");
        }
    }

    // ---- live settings ------------------------------------------------------------

    /// Per-output volume and mute take effect immediately (smoothly) while playing.
    #[test]
    fn live_volume_and_mute() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let set = settings(0, 100);
        let mut slave = Slave::new(cons, 2, 48_000, set.clone(), unity(), Arc::new(Stats::default())).unwrap();
        let mut phase = 0.0f32;
        let mut run = |ticks: usize, slave: &mut Slave| -> f32 {
            let mut pk = 0.0;
            for _ in 0..ticks {
                prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
                let mut out = vec![0.0f32; 480 * 2];
                slave.fill(&mut out);
                pk = peak(&out);
            }
            pk
        };
        let full = run(60, &mut slave);
        set.adjust_volume(-50);
        let half = run(30, &mut slave);
        assert!((half - full * 0.5).abs() < 0.03, "half {half} vs {full}");
        assert!(set.toggle_mute());
        assert!(run(30, &mut slave) < 0.001, "mute");
        assert!(!set.toggle_mute());
        assert!(run(30, &mut slave) > 0.2, "unmute");
    }

    /// Raising the delay live: a gap of about the difference, then audio resumes.
    #[test]
    fn live_delay_increase_gaps_then_resumes() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let set = settings(0, 100);
        let stats = Arc::new(Stats::default());
        let mut slave = Slave::new(cons, 2, 48_000, set.clone(), unity(), stats.clone()).unwrap();
        let mut phase = 0.0f32;
        let mut tick = |slave: &mut Slave| -> bool {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
            let mut out = vec![0.0f32; 480 * 2];
            slave.fill(&mut out);
            out.iter().any(|s| *s != 0.0)
        };
        for _ in 0..50 {
            tick(&mut slave);
        }
        assert!(tick(&mut slave), "should be playing before the change");
        set.adjust_delay(100); // +100 ms
        // The first tick may still play a little already-queued audio, so look at the
        // whole window: about 100 ms (10 ticks) of silence, then playback again.
        let played: Vec<bool> = (0..40).map(|_| tick(&mut slave)).collect();
        let silent = played.iter().filter(|p| !**p).count();
        assert!((7..=12).contains(&silent), "gap was {silent} ticks (10 ms each): {played:?}");
        assert!(*played.last().unwrap(), "never resumed: {played:?}");
        assert_eq!(
            stats.target_frames.load(Ordering::Relaxed) as usize,
            100 * 48_000 / 1000 + 48_000 / 50
        );
    }

    /// Lowering the delay live: surplus audio is dropped, playback never stops.
    #[test]
    fn live_delay_decrease_drops_audio_without_gap() {
        let rb = HeapRb::<f32>::new(48_000 * 2 * 2);
        let (mut prod, cons) = rb.split();
        let set = settings(200, 100);
        let stats = Arc::new(Stats::default());
        let mut slave = Slave::new(cons, 2, 48_000, set.clone(), unity(), stats.clone()).unwrap();
        let mut phase = 0.0f32;
        let mut tick = |slave: &mut Slave| -> bool {
            prod.push_slice(&sine_chunk(480, 2, &mut phase, 0.1));
            let mut out = vec![0.0f32; 480 * 2];
            slave.fill(&mut out);
            out.iter().any(|s| *s != 0.0)
        };
        for _ in 0..80 {
            tick(&mut slave);
        }
        set.adjust_delay(-200);
        for i in 0..15 {
            assert!(tick(&mut slave), "gap at tick {i} after lowering the delay");
        }
        let fill = stats.fill_frames.load(Ordering::Relaxed);
        let target = stats.target_frames.load(Ordering::Relaxed);
        assert_eq!(target as usize, 48_000 / 50);
        assert!(fill < target * 2, "ring still holds {fill} frames (target {target})");
    }

    #[test]
    fn settings_clamp() {
        let s = Settings::new(2000, 150);
        assert_eq!(s.delay_ms.load(Ordering::Relaxed), MAX_DELAY_MS);
        assert_eq!(s.volume_pct.load(Ordering::Relaxed), 100);
        assert_eq!(s.adjust_delay(-5000), 0);
        assert_eq!(s.adjust_delay(10), 10);
        assert_eq!(s.adjust_volume(10), 100);
        assert_eq!(s.adjust_volume(-500), 0);
    }
}