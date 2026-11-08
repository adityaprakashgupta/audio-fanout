//! Loopback capture from the virtual device, fanned out to N ring buffers.

use crate::devices::{open_by_id, Fmt};
use anyhow::Result;
use ringbuf::traits::{Observer, Producer};
use ringbuf::HeapProd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::w;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;

pub fn run(
    source_id: &str,
    fmt: &Fmt,
    mut sinks: Vec<HeapProd<f32>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let dev = open_by_id(source_id)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;

        // 100 ms buffer so polling every ~5 ms (Windows timer granularity may
        // make it ~15 ms) can never overflow it.
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            1_000_000,
            0,
            fmt.ptr(),
            None,
        )?;
        let cap: IAudioCaptureClient = client.GetService()?;

        let mut task = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
        client.Start()?;

        let silence = vec![0.0f32; 48_000 * fmt.channels]; // reusable zero buffer

        let mut scratch: Vec<f32> = Vec::with_capacity(48_000 * fmt.channels);

        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(2));
            let gain: f32 = if vol.GetMute()?.as_bool() {
                0.0
            } else {
                10f32.powf(vol.GetMasterVolumeLevel()? / 20.0) // dB -> linear, same curve Windows uses
            };
            loop {
                if cap.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;

                let n = frames as usize * fmt.channels;
                let is_silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let chunk: &[f32] = if is_silent {
                    &silence[..n.min(silence.len())]
                } else {
                    let src = std::slice::from_raw_parts(data as *const f32, n);
                    scratch.clear();
                    scratch.extend(src.iter().map(|s| s * gain));
                    &scratch
                };

                for s in sinks.iter_mut() {
                    // All-or-nothing: never push a partial frame, or channels misalign.
                    // A full ring means that output is stalled/dead; just drop for it.
                    if s.vacant_len() >= chunk.len() {
                        s.push_slice(chunk);
                    }
                }
                cap.ReleaseBuffer(frames)?;
            }
        }
        client.Stop()?;
    }
    Ok(())
}
