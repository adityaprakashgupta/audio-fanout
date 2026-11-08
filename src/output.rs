//! One render stream per target device, each on its own thread.

use crate::devices::{open_by_id, Fmt};
use anyhow::Result;
use ringbuf::traits::{Consumer, Observer};
use ringbuf::HeapCons;
use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{w, PCWSTR};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;

const CHUNK: usize = 256; // input frames per resampler call (~5 ms at 48 kHz)

pub fn run(
    id: &str,
    fmt: &Fmt,
    mut cons: HeapCons<f32>,
    delay_ms: u32,
    volume: f32,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let dev = open_by_id(id)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            0,
            0,
            fmt.ptr(),
            None,
        )?;
        let event = CreateEventW(None, false, false, PCWSTR::null())?;
        client.SetEventHandle(event)?;
        let render: IAudioRenderClient = client.GetService()?;
        let sv: ISimpleAudioVolume = client.GetService()?;
        sv.SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())?;
        let buf_frames = client.GetBufferSize()?;

        let mut task = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
        client.Start()?;

        let ch = fmt.channels;
        let rate = fmt.rate as usize;
        // Frames we keep buffered: user delay + 10 ms cushion. This is also the
        // set-point of the drift controller.
        let target = delay_ms as usize * rate / 1000 + rate / 50;

        let mut resampler = FastFixedIn::<f32>::new(1.0, 1.01, PolynomialDegree::Cubic, CHUNK, ch)?;
        let mut inter = vec![0.0f32; CHUNK * ch];
        let mut inbuf = vec![vec![0.0f32; CHUNK]; ch];
        let mut outbuf = vec![vec![0.0f32; resampler.output_frames_max()]; ch];
        let mut pending: VecDeque<f32> = VecDeque::with_capacity(rate * ch / 10);

        let mut primed = false;
        let mut err_avg = 0.0f64;
        let mut ticks = 0u32;

        while !stop.load(Ordering::Relaxed) {
            WaitForSingleObject(event, 200);
            let pad = client.GetCurrentPadding()?;
            let avail = (buf_frames - pad) as usize;
            if avail == 0 {
                continue;
            }
            let data = render.GetBuffer(avail as u32)? as *mut f32;
            let out = std::slice::from_raw_parts_mut(data, avail * ch);

            // Safety net only: if we are wildly behind, jump (this can click).
            let fill = cons.occupied_len() / ch;
            if fill > target + rate / 10 {
                cons.skip((fill - target) * ch);
            }
            // (Re)prime: wait until the delay is buffered before playing.
            if !primed && cons.occupied_len() / ch >= target {
                primed = true;
            }

            // Keep `pending` (resampled, interleaved) topped up.
            while primed && pending.len() < out.len() && cons.occupied_len() >= CHUNK * ch {
                // Drift controller: ring too full => consume input faster
                // (ratio < 1); too empty => slower (ratio > 1).
                let fill = (cons.occupied_len() / ch) as f64;
                let err = (fill - target as f64) / target as f64;
                err_avg += 0.02 * (err - err_avg);
                let rel = 1.0 - (0.002 * err_avg).clamp(-0.005, 0.005);
                resampler.set_resample_ratio_relative(rel, true)?;

                cons.pop_slice(&mut inter);
                for f in 0..CHUNK {
                    for c in 0..ch {
                        inbuf[c][f] = inter[f * ch + c];
                    }
                }
                let (_, n) = resampler.process_into_buffer(&inbuf, &mut outbuf, None)?;
                for f in 0..n {
                    for c in 0..ch {
                        pending.push_back(outbuf[c][f]);
                    }
                }

                // Debug: every ~10 s. Remove later (printing on this thread can glitch).
                // ticks += 1;
                // if ticks % 2000 == 0 {
                //     eprintln!(
                //         "[..{}] avg_fill={:.0}/{target} ratio={rel:.6}",
                //         &id[id.len().saturating_sub(5)..],
                //         target as f64 * (1.0 + err_avg)
                //     );
                // }
            }

            let have = pending.len().min(out.len());
            for s in out[..have].iter_mut() {
                *s = pending.pop_front().unwrap();
            }
            out[have..].fill(0.0);
            if have < out.len() {
                primed = false; // underrun: rebuild the delay before playing again
            }
            render.ReleaseBuffer(avail as u32, 0)?;
        }
        client.Stop()?;
    }
    Ok(())
}
