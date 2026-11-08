//! audio-fanout (cross-platform): capture one source and play it on any number
//! of output devices, keeping them locked together.
//!
//! The source is an INPUT device on every OS:
//!   Windows : "CABLE Output" (VB-Cable)  -- or, as a convenience, any output
//!             device by name, which is captured via WASAPI loopback
//!   macOS   : "BlackHole 2ch" (or any virtual device that shows up as an input)
//!   Linux   : a null sink's monitor source (see README notes in the reply)
//!
//!   audio-fanout --list
//!   audio-fanout --source "CABLE Output" --out "Speakers:180" --out "Headphones:0:50"
//!
//! Names are case-insensitive substrings. --out "Name[:delay_ms[:volume_percent]]"

mod slave;

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig, SupportedStreamConfigRange};
use ringbuf::traits::{Observer, Producer, Split};
use ringbuf::HeapRb;
use slave::{Slave, Stats};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn name_of(d: &Device) -> String {
    d.name().unwrap_or_else(|_| "<unknown>".into())
}

fn find(devs: impl Iterator<Item = Device>, needle: &str) -> Option<Device> {
    let n = needle.to_lowercase();
    devs.into_iter().find(|d| name_of(d).to_lowercase().contains(&n))
}

/// Pick an f32 config at the device's default rate/channels.
/// `from_output`: read the *output* side of the device (needed for WASAPI loopback sources).
fn f32_config(dev: &Device, output_side: bool) -> Result<StreamConfig> {
    let def = if output_side { dev.default_output_config()? } else { dev.default_input_config()? };
    if def.sample_format() == SampleFormat::F32 {
        return Ok(def.config());
    }
    let mut ranges: Vec<SupportedStreamConfigRange> = if output_side {
        dev.supported_output_configs()?.collect()
    } else {
        dev.supported_input_configs()?.collect()
    };
    let r = ranges
        .drain(..)
        .find(|r| {
            r.sample_format() == SampleFormat::F32
                && r.channels() == def.channels()
                && r.min_sample_rate() <= def.sample_rate()
                && def.sample_rate() <= r.max_sample_rate()
        })
        .ok_or_else(|| anyhow!("\"{}\" offers no f32 format", name_of(dev)))?;
    Ok(r.with_sample_rate(def.sample_rate()).config())
}

fn main() -> Result<()> {
    let host = cpal::default_host();

    let mut source: Option<String> = None;
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

    // ---- source -----------------------------------------------------------
    let (src_dev, loopback) = match find(host.input_devices()?, &source) {
        Some(d) => (d, false),
        None if cfg!(windows) => match find(host.output_devices()?, &source) {
            Some(d) => (d, true), // cpal's WASAPI backend captures output devices as loopback
            None => bail!("no input or output device matching \"{source}\" (try --list)"),
        },
        None => bail!("no input device matching \"{source}\" (try --list)"),
    };
    let src_cfg = f32_config(&src_dev, loopback)?;
    let (src_ch, src_rate) = (src_cfg.channels as usize, src_cfg.sample_rate.0);
    println!("source : {}{}", name_of(&src_dev), if loopback { " (loopback)" } else { "" });
    println!("format : {src_rate} Hz, {src_ch} ch, f32");

    // ---- outputs ----------------------------------------------------------
    let all_out: Vec<Device> = host.output_devices()?.collect();
    let mut producers = Vec::new();
    let mut streams = Vec::new();
    let mut all_stats: Vec<(String, Arc<Stats>)> = Vec::new();

    for (needle, delay_ms, volume) in &outs {
        let n = needle.to_lowercase();
        let dev = all_out
            .iter()
            .find(|d| name_of(d).to_lowercase().contains(&n))
            .ok_or_else(|| anyhow!("no output device matching \"{needle}\" (try --list)"))?;
        let label = name_of(dev);
        if loopback && label == name_of(&src_dev) {
            bail!("\"{label}\" is the loopback source; routing it to itself would feed back");
        }
        let cfg = f32_config(dev, true)?;
        println!(
            "output : {label} ({} Hz, {} ch, +{delay_ms} ms, {}%)",
            cfg.sample_rate.0,
            cfg.channels,
            (volume * 100.0) as u32
        );

        let rb = HeapRb::<f32>::new(src_rate as usize * src_ch * 2); // 2 s headroom
        let (prod, cons) = rb.split();
        producers.push(prod);

        let stats = Arc::new(Stats::default());
        all_stats.push((label.clone(), stats.clone()));
        let mut slave = Slave::new(
            cons,
            src_ch,
            src_rate,
            cfg.channels as usize,
            cfg.sample_rate.0,
            *delay_ms,
            volume.clamp(0.0, 1.0),
            stats,
        )?;
        let err_label = label.clone();
        let stream = dev.build_output_stream(
            &cfg,
            move |data: &mut [f32], _| slave.fill(data),
            move |e| eprintln!("[{err_label}] stream error: {e}"),
            None,
        )?;
        streams.push(stream);
    }

    // Start outputs first so they are waiting when audio begins to flow.
    for s in &streams {
        s.play()?;
    }
    let in_stream = src_dev.build_input_stream(
        &src_cfg,
        move |data: &[f32], _| {
            for p in producers.iter_mut() {
                // All-or-nothing: never push a partial frame (channels would misalign).
                if p.vacant_len() >= data.len() {
                    p.push_slice(data);
                }
            }
        },
        |e| eprintln!("[source] stream error: {e}"),
        None,
    )?;
    in_stream.play()?;

    println!("\nRunning. Send your apps' audio to the source device. Press Enter to quit.");
    let stop = Arc::new(AtomicBool::new(false));
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
                    "[{label}] fill={}/{} frames, ratio={:+} ppm",
                    s.fill_frames.load(Ordering::Relaxed),
                    s.target_frames.load(Ordering::Relaxed),
                    s.ratio_ppm.load(Ordering::Relaxed)
                );
            }
        }
    }
    Ok(())
}