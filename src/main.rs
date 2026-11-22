//! audio-fanout: capture one source and play it on any number of output devices,
//! keeping them locked together and reconnecting any that drop.
//!
//! Default: an interactive terminal UI. `--no-tui` runs headless (same engine).
//!
//!   audio-fanout                                   # TUI, pick the source on screen
//!   audio-fanout --source "CABLE Output"           # TUI, add outputs with 'a'
//!   audio-fanout --source "CABLE Output" --out "Speakers:180" --out "Headphones:0:50"
//!   audio-fanout --no-tui --source ... --out ...   # headless, logs to stdout
//!   audio-fanout --source ... --send 192.168.1.5   # also stream to a network receiver
//!   audio-fanout --receive                         # be a network receiver (speaker)
//!   audio-fanout --list
//!
//! Names are case-insensitive substrings. --out "Name[:delay_ms[:volume_percent]]"

mod connectors;
mod devices;
mod discovery;
mod engine;
mod log;
mod netout;
mod proto;
mod receiver;
mod slave;
mod supervisor;
mod tui;
mod volume;
mod watch;

#[cfg(test)]
mod net_tests;

use anyhow::{anyhow, bail, Result};
use cpal::traits::HostTrait;
use devices::name_of;
use engine::Engine;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use watch::Watcher;

struct Options {
    source: Option<String>,
    outs: Vec<(String, u32, u32)>, // name (or net:HOST[:PORT]), delay ms, volume %
    follow_volume: bool,
    tui: bool,
    // receiver mode
    receive: bool,
    port: u16,
    device: String,
    jitter_ms: u32,
    name: String,
}

const HELP: &str = "\
audio-fanout: play one audio source on many output devices, in sync.

USAGE:
  audio-fanout [--source NAME] [--out NAME[:DELAY_MS[:VOLUME_PCT]]]... [--no-tui] [--no-follow-volume]
  audio-fanout [--source NAME] --send HOST[:PORT][,DELAY_MS[,VOLUME_PCT]]...     stream to network receivers
  audio-fanout --receive [--port N] [--device NAME] [--jitter-ms N] [--name NAME]   be a network receiver
  audio-fanout --list

Without --source the TUI asks you to pick one. The TUI shows its keys on screen.";

fn parse_args() -> Result<Option<Options>> {
    let host = cpal::default_host();
    let mut o = Options {
        source: None,
        outs: Vec::new(),
        follow_volume: true,
        tui: true,
        receive: false,
        port: proto::DEFAULT_PORT,
        device: String::new(),
        jitter_ms: 30,
        name: String::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(None);
            }
            "--list" => {
                println!("INPUT devices (use as --source):");
                for d in host.input_devices()? {
                    println!("  {}", name_of(&d));
                }
                println!("\nOUTPUT devices (use as --out; on Windows also valid as --source = loopback):");
                for d in host.output_devices()? {
                    println!("  {}", name_of(&d));
                }
                return Ok(None);
            }
            "--source" => o.source = args.next(),
            "--no-follow-volume" => o.follow_volume = false,
            "--no-tui" => o.tui = false,
            "--receive" => o.receive = true,
            "--port" => {
                o.port = args.next().and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("--port needs a number"))?
            }
            "--device" => o.device = args.next().ok_or_else(|| anyhow!("--device needs a name"))?,
            "--name" => o.name = args.next().ok_or_else(|| anyhow!("--name needs a value"))?,
            "--jitter-ms" => {
                o.jitter_ms = args.next().and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("--jitter-ms needs a number"))?
            }
            "--send" => {
                let v = args.next().ok_or_else(|| anyhow!("--send needs HOST[:PORT][,DELAY_MS[,VOLUME_PCT]]"))?;
                let mut it = v.split(',');
                let host = it.next().unwrap().trim().to_string();
                if host.is_empty() {
                    bail!("--send needs a host");
                }
                let delay = it.next().and_then(|d| d.trim().parse().ok()).unwrap_or(0u32);
                let vol = it.next().and_then(|d| d.trim().parse::<f32>().ok()).unwrap_or(100.0);
                o.outs.push((format!("net:{host}"), delay, vol.round().clamp(0.0, 100.0) as u32));
            }
            "--out" => {
                let v = args.next().ok_or_else(|| anyhow!("--out needs a value"))?;
                let mut it = v.split(':');
                let name = it.next().unwrap().to_string();
                let delay = it.next().and_then(|d| d.parse().ok()).unwrap_or(0u32);
                let vol = it.next().and_then(|d| d.parse::<f32>().ok()).unwrap_or(100.0);
                o.outs.push((name, delay, vol.round().clamp(0.0, 100.0) as u32));
            }
            other => bail!("unknown argument {other} (try --help)"),
        }
    }
    Ok(Some(o))
}

fn run_headless(o: Options) -> Result<()> {
    log::set_echo(true);
    let source = o.source.ok_or_else(|| anyhow!("--source is required with --no-tui (see --list)"))?;
    if o.outs.is_empty() {
        bail!("give at least one --out with --no-tui");
    }
    let mut engine = Engine::start(&source, o.follow_volume)?;
    log::log(format!("source: {} ({} Hz, {} ch)", engine.source_name, engine.src_rate, engine.src_ch));
    for (name, delay, vol) in &o.outs {
        engine.add_output(name, *delay, *vol)?;
    }
    println!("\nRunning. Send your apps' audio to the source device. Press Enter to quit.");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    engine.shutdown();
    Ok(())
}

/// `--receive`: be a network speaker. Controlled entirely from the broadcaster.
fn run_receive(o: Options) -> Result<()> {
    log::set_echo(true);
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::stdin().read_line(&mut s);
            stop.store(true, Ordering::Relaxed);
        });
    }
    println!("Receiver running. Press Enter to quit.");
    receiver::run_receiver(
        receiver::ReceiveOptions {
            port: o.port,
            name: o.name,
            device: o.device,
            jitter_ms: o.jitter_ms,
            follow_volume: o.follow_volume,
        },
        stop,
    )
}

fn run_tui(o: Options) -> Result<()> {
    log::set_echo(false); // anything printed would tear the screen
    let mut terminal = ratatui::try_init().map_err(|e| {
        anyhow!("cannot start the terminal UI ({e}); run it in a real terminal, or use --no-tui")
    })?;
    let watcher = Watcher::start();
    let result = (|| -> Result<()> {
        let source = match o.source {
            Some(s) => s,
            None => match tui::pick_source(&mut terminal, &watcher)? {
                Some(s) => s,
                None => return Ok(()),
            },
        };
        let mut engine = Engine::start(&source, o.follow_volume)?;
        for (name, delay, vol) in &o.outs {
            engine.add_output(name, *delay, *vol)?;
        }
        tui::run(&mut terminal, &mut engine, &watcher)?;
        engine.shutdown();
        Ok(())
    })();
    ratatui::restore();
    result
}

fn main() -> Result<()> {
    // Windows sleeps in ~15 ms steps by default, which would make the network sender
    // burst. Ask for 1 ms timers for the life of the process.
    #[cfg(windows)]
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }

    let Some(o) = parse_args()? else { return Ok(()) };
    if o.receive {
        run_receive(o)
    } else if o.tui {
        run_tui(o)
    } else {
        run_headless(o)
    }
}
