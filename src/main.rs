//! audio-fanout (cross-platform): capture one source and play it on any number of
//! output devices, keeping them locked together and reconnecting any that drop.
//!
//! The source is an INPUT device on every OS:
//!   Windows : "CABLE Output" (VB-Cable) -- or, as a convenience, any output device
//!             by name, which is captured via WASAPI loopback
//!   macOS   : "BlackHole 2ch" (or any virtual device that shows up as an input)
//!   Linux   : a null sink's monitor source
//!
//!   audio-fanout --list
//!   audio-fanout --source "CABLE Output" --out "Speakers:180" --out "Headphones:0:50"
//!
//! Names are case-insensitive substrings. --out "Name[:delay_ms[:volume_percent]]"
//! Outputs that are missing at startup, or that disconnect later, are retried
//! automatically; the source must exist at startup (its format sizes the buffers).

mod connectors;
mod devices;
mod slave;
mod supervisor;
mod volume;

use anyhow::{anyhow, bail, Result};
use connectors::{OutputConnector, SourceConnector};
use cpal::traits::HostTrait;
use devices::{f32_config, name_of, resolve_source};
use ringbuf::traits::Split;
use ringbuf::HeapRb;
use slave::{Slave, Stats};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use supervisor::Timing;

fn main() -> Result<()> {
    let host = cpal::default_host();

    let mut source: Option<String> = None;
    let mut follow_volume = true;
    let mut outs: Vec<(String, u32, f32)> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => {
                println!("INPUT devices (use as --source):");
                for d in host.input_devices()? {
                    println!("  {}", name_of(&d));
                }
                println!("\nOUTPUT devices (use as --out; on Windows also valid as --source = loopback):");
                for d in host.output_devices()? {
                    println!("  {}", name_of(&d));
                }
                return Ok(());
            }
            "--source" => source = args.next(),
            "--no-follow-volume" => follow_volume = false,
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

    // The source must exist now: its format sizes every ring buffer and resampler.
    let (src_dev, loopback) = resolve_source(&host, &source)?;
    let src_cfg = f32_config(&src_dev, loopback)?;
    let (src_ch, src_rate) = (src_cfg.channels as usize, src_cfg.sample_rate.0);
    let src_name = name_of(&src_dev);
    drop(src_dev);

    let stop = Arc::new(AtomicBool::new(false));
    let master = if follow_volume {
        println!("volume : following the system default-output volume (--no-follow-volume to disable)");
        volume::start(stop.clone())
    } else {
        Arc::new(AtomicU32::new(1.0f32.to_bits()))
    };

    let mut handles = Vec::new();
    let mut producers = Vec::new();
    let mut all_stats: Vec<(String, Arc<Stats>)> = Vec::new();

    for (needle, delay_ms, volume) in &outs {
        let rb = HeapRb::<f32>::new(src_rate as usize * src_ch * 2); // 2 s headroom
        let (prod, cons) = rb.split();
        producers.push(prod);

        let stats = Arc::new(Stats::default());
        all_stats.push((needle.clone(), stats.clone()));
        let slave = Arc::new(Mutex::new(Slave::new(
            cons,
            src_ch,
            src_rate,
            *delay_ms,
            volume.clamp(0.0, 1.0),
            master.clone(),
            stats.clone(),
        )?));
        let forbid = loopback.then(|| src_name.clone());
        let n = needle.clone();
        println!("output : {needle} (+{delay_ms} ms, {}%)", (volume * 100.0) as u32);
        handles.push(supervisor::spawn(
            needle.clone(),
            move || OutputConnector { needle: n, slave, forbid },
            stop.clone(),
            stats,
            Timing::default(),
        ));
    }

    // The source supervisor owns capture; outputs keep their buffers while it is down,
    // play silence if they run dry, and re-prime together when audio returns.
    let producers = Arc::new(Mutex::new(producers));
    handles.push(supervisor::spawn(
        "source".into(),
        {
            let needle = source.clone();
            move || SourceConnector {
                needle,
                rate: src_rate,
                channels: src_cfg.channels,
                producers,
            }
        },
        stop.clone(),
        Arc::new(Stats::default()),
        Timing::default(),
    ));

    println!("\nRunning. Send your apps' audio to the source device. Press Enter to quit.");
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
            stop.store(true, Ordering::Relaxed);
        });
    }
    let debug = std::env::var_os("FANOUT_DEBUG").is_some();
    let mut tick = 0u32;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        tick += 1;
        if debug && tick % 10 == 0 {
            for (label, s) in &all_stats {
                eprintln!(
                    "[{label}] {} fill={}/{} frames, ratio={:+} ppm, reconnects={}",
                    if s.connected.load(Ordering::Relaxed) { "up  " } else { "DOWN" },
                    s.fill_frames.load(Ordering::Relaxed),
                    s.target_frames.load(Ordering::Relaxed),
                    s.ratio_ppm.load(Ordering::Relaxed),
                    s.reconnects.load(Ordering::Relaxed)
                );
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}