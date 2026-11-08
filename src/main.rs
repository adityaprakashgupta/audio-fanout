//! audio-fanout: capture a (virtual) render device via WASAPI loopback and
//! replay it on any number of real output devices.
//!
//! Usage:
//!   audio-fanout --list
//!   audio-fanout --source "CABLE Output" --out "Speakers:180" --out "Headphones:0:50"
//!
//! Names are case-insensitive substrings. --out "Name[:delay_ms[:volume_percent]]"

mod capture;
mod devices;
mod output;

use anyhow::{anyhow, bail, Result};
use devices::{enumerate_render, open_by_id, Endpoint, Fmt};
use ringbuf::traits::Split;
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use windows::Win32::Media::Audio::IAudioClient;
use windows::Win32::System::Com::*;
use windows::Win32::Media::timeBeginPeriod;

fn find<'a>(eps: &'a [Endpoint], needle: &str) -> Result<&'a Endpoint> {
    let n = needle.to_lowercase();
    eps.iter()
        .find(|e| e.name.to_lowercase().contains(&n))
        .ok_or_else(|| anyhow!("no active output device matching \"{needle}\" (try --list)"))
}

fn main() -> Result<()> {
    unsafe { timeBeginPeriod(1) };
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let eps = enumerate_render()?;

    let mut source: Option<String> = None;
    let mut outs: Vec<(String, u32, f32)> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => {
                for e in &eps {
                    println!("{}\n    {}", e.name, e.id);
                }
                return Ok(());
            }
            "--source" => source = args.next(),
            "--out" => {
                let v = args.next().ok_or_else(|| anyhow!("--out needs a value"))?;
                let mut it = v.split(':');
                let name = it.next().unwrap().to_string();
                let delay = it.next().and_then(|d| d.parse().ok()).unwrap_or(0u32);
                let vol = it.next().and_then(|d| d.parse::<f32>().ok()).unwrap_or(100.0) / 100.0;
                outs.push((name, delay, vol));
            }
            other => bail!("unknown argument {other}"),
        }
    }
    let source = source.ok_or_else(|| anyhow!("--source is required (see --list)"))?;
    if outs.is_empty() {
        bail!("give at least one --out");
    }

    let src = find(&eps, &source)?;
    println!("source : {}", src.name);

    // Read the source's mix format once; every output is created with this format.
    let fmt = unsafe {
        let dev = open_by_id(&src.id)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        Fmt::from_mix(&client)?
    };
    println!("format : {} Hz, {} ch, f32", fmt.rate, fmt.channels);

    let stop = Arc::new(AtomicBool::new(false));
    let mut producers = Vec::new();
    let mut handles = Vec::new();

    for (name, delay_ms, volume) in outs {
        let ep = find(&eps, &name)?;
        if ep.id == src.id {
            bail!("\"{}\" is the source device; routing it to itself would feed back", ep.name);
        }
        println!("output : {} (+{} ms, {}%)", ep.name, delay_ms, (volume * 100.0) as u32);

        // 2 seconds of headroom per output.
        let rb = HeapRb::<f32>::new(fmt.rate as usize * fmt.channels * 2);
        let (prod, cons) = rb.split();
        producers.push(prod);

        let (id, label, fmt, stop) = (ep.id.clone(), ep.name.clone(), fmt.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            // One dead output (e.g. Bluetooth drops) must not take down the others.
            if let Err(e) = output::run(&id, &fmt, cons, delay_ms, volume, stop) {
                eprintln!("[{label}] stopped: {e}");
            }
        }));
    }

    {
        let (id, fmt, stop) = (src.id.clone(), fmt.clone(), stop.clone());
        handles.push(thread::spawn(move || {
            if let Err(e) = capture::run(&id, &fmt, producers, stop) {
                eprintln!("[capture] stopped: {e}");
            }
        }));
    }

    println!("\nRunning. Set \"{}\" as your default output. Press Enter to quit.", src.name);
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
