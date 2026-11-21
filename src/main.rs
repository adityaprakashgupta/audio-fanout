//! audio-fanout: capture one source and play it on any number of output devices,
//! keeping them locked together and reconnecting any that drop.
//!
//! Default: an interactive terminal UI. `--no-tui` runs headless (same engine).
//!
//!   audio-fanout                                   # TUI, pick the source on screen
//!   audio-fanout --source "CABLE Output"           # TUI, add outputs with 'a'
//!   audio-fanout --source "CABLE Output" --out "Speakers:180" --out "Headphones:0:50"
//!   audio-fanout --no-tui --source ... --out ...   # headless, logs to stdout
//!   audio-fanout --list
//!
//! Names are case-insensitive substrings. --out "Name[:delay_ms[:volume_percent]]"

mod connectors;
mod devices;
mod engine;
mod log;
mod slave;
mod supervisor;
mod tui;
mod volume;
mod watch;

use anyhow::{anyhow, bail, Result};
use cpal::traits::HostTrait;
use devices::name_of;
use engine::Engine;
use watch::Watcher;

struct Options {
    source: Option<String>,
    outs: Vec<(String, u32, u32)>, // name, delay ms, volume %
    follow_volume: bool,
    tui: bool,
}

const HELP: &str = "\
audio-fanout: play one audio source on many output devices, in sync.

USAGE:
  audio-fanout [--source NAME] [--out NAME[:DELAY_MS[:VOLUME_PCT]]]... [--no-tui] [--no-follow-volume]
  audio-fanout --list

Without --source the TUI asks you to pick one. The TUI shows its keys on screen.";

fn parse_args() -> Result<Option<Options>> {
    let host = cpal::default_host();
    let mut o = Options { source: None, outs: Vec::new(), follow_volume: true, tui: true };
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
    let Some(o) = parse_args()? else { return Ok(()) };
    if o.tui {
        run_tui(o)
    } else {
        run_headless(o)
    }
}