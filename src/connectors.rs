//! cpal-backed connectors: one for the source stream, one per output stream.

use crate::devices::{f32_config, find, loops_back_to, name_of, resolve_source};
use crate::engine::SinkList;
use crate::log::log;
use crate::slave::Slave;
use crate::supervisor::Connector;
use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use ringbuf::traits::{Observer, Producer};
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
    /// Name of the source device: refuse outputs that would feed back into it.
    pub source_name: String,
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
        if loops_back_to(&self.source_name, &name) {
            bail!("\"{name}\" would feed back into the source");
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
                log(format!("[{label}] stream error: {e}"));
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
    /// The live list of outputs' ring-buffer producers, swapped atomically when the
    /// user adds or removes an output (the input callback never blocks on it).
    pub sinks: SinkList,
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
        let (f2, sinks) = (failed.clone(), self.sinks.clone());
        let stream = dev.build_input_stream(
            &cfg,
            move |data: &[f32], _| {
                let list = sinks.load();
                for sink in list.iter() {
                    // The lock is never contended (only this callback touches a producer).
                    if let Ok(mut p) = sink.prod.try_lock() {
                        // All-or-nothing: never push a partial frame (channels would misalign).
                        if p.vacant_len() >= data.len() {
                            p.push_slice(data);
                        }
                    }
                }
            },
            move |e| {
                log(format!("[source] stream error: {e}"));
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