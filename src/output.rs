//! One render stream per target device, each on its own thread.

use crate::devices::{open_by_id, Fmt};
use anyhow::Result;
use ringbuf::traits::{Consumer, Observer};
use ringbuf::HeapCons;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{w, PCWSTR};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;

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

        // AUTOCONVERTPCM + SRC_DEFAULT_QUALITY: Windows converts our (source) format
        // to whatever this device runs at, so we skip per-device format handling.
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
        // Samples we want buffered before playback starts: user delay + 20 ms cushion.
        let prime = (delay_ms as usize * rate / 1000 + rate / 200) * ch;
        // If the ring grows past prime + 50 ms, this clock is slower than the source: trim.
        let max_fill = prime + (rate / 20) * ch;
        let mut primed = false;

        while !stop.load(Ordering::Relaxed) {
            WaitForSingleObject(event, 200);
            let pad = client.GetCurrentPadding()?;
            let avail = (buf_frames - pad) as usize;
            if avail == 0 {
                continue;
            }
            let data = render.GetBuffer(avail as u32)? as *mut f32;
            let out = std::slice::from_raw_parts_mut(data, avail * ch);

            // Drift correction (crude): drop the excess. Swap for rubato later.
            let occ = cons.occupied_len();
            if occ > max_fill {
                cons.skip(occ - prime);
            }
            // (Re)prime: after silence or an underrun, rebuild the delay before playing.
            if !primed && cons.occupied_len() >= prime {
                primed = true;
            }

            let got = if primed { cons.pop_slice(out) } else { 0 };
            if got < out.len() {
                out[got..].fill(0.0);
                primed = false;
            }
            render.ReleaseBuffer(avail as u32, 0)?;
        }
        client.Stop()?;
    }
    Ok(())
}
