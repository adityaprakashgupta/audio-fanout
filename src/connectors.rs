//! cpal-backed connectors: one for the source stream, one per output stream.

use crate::devices::{f32_config, find, name_of, resolve_source};
use crate::slave::Slave;
use crate::supervisor::Connector;
use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use ringbuf::traits::{Observer, Producer};
use ringbuf::HeapProd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often we double-check that the device still shows up in the OS device list,
/// as a backstop for platforms that don't raise a stream error on removal.
const PRESENCE_CHECK: Duration = Duration::from_secs(2);

// ---- output --------------------------------------------------------------------

pub struct OutputConnector {
    pub needle: String,
    /// Shared with the audio callback, which only ever `try_lock`s it (never blocks).
    pub slave: Arc<Mutex<Slave>>,
    /// If set, refuse this device name (it is the loopback source: would feed back).
    pub forbid: Option<String>,
}

pub struct OutConn {
    _stream: Stream, // dropping it stops playback
    failed: Arc<AtomicBool>,
    name: String,
    last_check: Instant,
    desc: String,
}

impl Connector for OutputConnector {
    type Conn = OutConn;

    fn connect(&mut self) -> Result<OutConn> {
        let host = cpal::default_host();
        let dev = find(host.output_devices()?, &self.needle)
            .ok_or_else(|| anyhow!("no output device matching \"{}\"", self.needle))?;
        let name = name_of(&dev);
        if self.forbid.as_deref() == Some(name.as_str()) {
            bail!("\"{name}\" is the loopback source; routing it to itself would feed back");
        }
        let cfg = f32_config(&dev, true)?;

        // The previous stream is already dropped, so no callback is running: safe to
        // flush stale audio and adapt to this connection's format before starting.
        self.slave
            .lock()
            .map_err(|_| anyhow!("slave lock poisoned"))?
            .reconfigure(cfg.channels as usize, cfg.sample_rate.0)?;

        let failed = Arc::new(AtomicBool::new(false));
        let (f2, label) = (failed.clone(), name.clone());
        let slave = self.slave.clone();
        let stream = dev.build_output_stream(
            &cfg,
            move |data: &mut [f32], _| match slave.try_lock() {
                Ok(mut s) => s.fill(data),
                Err(_) => data.fill(0.0), // supervisor is mid-reconfigure: stay silent
            },
            move |e| {
                eprintln!("[{label}] stream error: {e}");
                f2.store(true, Ordering::Relaxed);
            },
            None,
        )?;
        stream.play()?;
        let desc = format!("{name} ({} Hz, {} ch)", cfg.sample_rate.0, cfg.channels);
        Ok(OutConn { _stream: stream, failed, name, last_check: Instant::now(), desc })
    }

    fn is_alive(&mut self, c: &mut OutConn) -> bool {
        if c.failed.load(Ordering::Relaxed) {
            return false;
        }
        if c.last_check.elapsed() >= PRESENCE_CHECK {
            c.last_check = Instant::now();
            if let Ok(mut devs) = cpal::default_host().output_devices() {
                if !devs.any(|d| name_of(&d) == c.name) {
                    return false;
                }
            }
        }
        true
    }

    fn describe(&self, c: &OutConn) -> String {
        c.desc.clone()
    }
}

// ---- source --------------------------------------------------------------------

pub struct SourceConnector {
    pub needle: String,
    /// Expected format. If the source comes back different, the ring buffers (sized
    /// for the original format) no longer match, so we refuse and ask for a restart.
    pub rate: u32,
    pub channels: u16,
    /// Producers for every output's ring buffer, written by the input callback.
    pub producers: Arc<Mutex<Vec<HeapProd<f32>>>>,
}

pub struct SrcConn {
    _stream: Stream,
    failed: Arc<AtomicBool>,
    name: String,
    loopback: bool,
    last_check: Instant,
    desc: String,
}

impl Connector for SourceConnector {
    type Conn = SrcConn;

    fn connect(&mut self) -> Result<SrcConn> {
        let host = cpal::default_host();
        let (dev, loopback) = resolve_source(&host, &self.needle)?;
        let cfg = f32_config(&dev, loopback)?;
        if cfg.sample_rate.0 != self.rate || cfg.channels != self.channels {
            bail!(
                "source came back as {} Hz / {} ch but was {} Hz / {} ch: restart audio-fanout",
                cfg.sample_rate.0, cfg.channels, self.rate, self.channels
            );
        }
        let name = name_of(&dev);
        let failed = Arc::new(AtomicBool::new(false));
        let (f2, prods) = (failed.clone(), self.producers.clone());
        let stream = dev.build_input_stream(
            &cfg,
            move |data: &[f32], _| {
                if let Ok(mut ps) = prods.try_lock() {
                    for p in ps.iter_mut() {
                        // All-or-nothing: never push a partial frame (channels would misalign).
                        if p.vacant_len() >= data.len() {
                            p.push_slice(data);
                        }
                    }
                }
            },
            move |e| {
                eprintln!("[source] stream error: {e}");
                f2.store(true, Ordering::Relaxed);
            },
            None,
        )?;
        stream.play()?;
        let desc = format!(
            "{name}{} ({} Hz, {} ch)",
            if loopback { " (loopback)" } else { "" },
            cfg.sample_rate.0,
            cfg.channels
        );
        Ok(SrcConn { _stream: stream, failed, name, loopback, last_check: Instant::now(), desc })
    }

    fn is_alive(&mut self, c: &mut SrcConn) -> bool {
        if c.failed.load(Ordering::Relaxed) {
            return false;
        }
        if c.last_check.elapsed() >= PRESENCE_CHECK {
            c.last_check = Instant::now();
            let host = cpal::default_host();
            let list = if c.loopback { host.output_devices() } else { host.input_devices() };
            if let Ok(mut devs) = list {
                if !devs.any(|d| name_of(&d) == c.name) {
                    return false;
                }
            }
        }
        true
    }

    fn describe(&self, c: &SrcConn) -> String {
        c.desc.clone()
    }
}