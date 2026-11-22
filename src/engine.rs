//! The engine: owns the source, the volume follower and every output, and lets the
//! UI add and remove outputs and change their settings while audio is playing.

use crate::connectors::{OutputConnector, SourceConnector};
use crate::devices::{f32_config, name_of, resolve_output_name, resolve_source};
use crate::log::log;
use crate::netout::{self, NetConnector, NetSender, NetStats};
use crate::slave::{Settings, Slave, Stats};
use crate::supervisor::{self, Timing};
use crate::volume;
use anyhow::{bail, Result};
use arc_swap::ArcSwap;
use ringbuf::traits::Split;
use ringbuf::{HeapProd, HeapRb};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// One output's ring-buffer producer, written by the source's input callback.
pub struct Sink {
    pub id: u64,
    pub prod: Mutex<HeapProd<f32>>,
}

/// Lock-free swappable list: adding/removing an output never stalls the audio thread.
pub type SinkList = Arc<ArcSwap<Vec<Arc<Sink>>>>;

pub struct OutputHandle {
    pub id: u64,
    pub name: String,
    pub settings: Arc<Settings>,
    pub stats: Arc<Stats>,
    /// Present for network receivers: what the receiver reports back.
    pub net: Option<Arc<NetStats>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

pub struct Engine {
    pub source_name: String,
    pub src_rate: u32,
    pub src_ch: usize,
    master: Arc<AtomicU32>,
    follow: Arc<AtomicBool>,
    sinks: SinkList,
    outputs: Vec<OutputHandle>,
    retiring: Vec<JoinHandle<()>>,
    next_id: u64,
    source_stats: Arc<Stats>,
    source_stop: Arc<AtomicBool>,
    source_join: Option<JoinHandle<()>>,
    global_stop: Arc<AtomicBool>,
}

impl Engine {
    /// The source must exist now: its format sizes every ring buffer and resampler.
    pub fn start(source_needle: &str, follow_volume: bool) -> Result<Engine> {
        let host = cpal::default_host();
        let (dev, loopback) = resolve_source(&host, source_needle)?;
        let cfg = f32_config(&dev, loopback)?;
        let source_name = name_of(&dev);
        drop(dev);
        let (src_ch, src_rate) = (cfg.channels as usize, cfg.sample_rate.0);

        let global_stop = Arc::new(AtomicBool::new(false));
        let follow = Arc::new(AtomicBool::new(follow_volume));
        let master = volume::start(global_stop.clone(), follow.clone());

        let sinks: SinkList = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let source_stats = Arc::new(Stats::default());
        let source_stop = Arc::new(AtomicBool::new(false));
        let source_join = supervisor::spawn(
            "source".into(),
            {
                let (needle, sinks, channels) = (source_name.clone(), sinks.clone(), cfg.channels);
                move || SourceConnector { needle, rate: src_rate, channels, sinks }
            },
            source_stop.clone(),
            source_stats.clone(),
            Timing::default(),
        );
        Ok(Engine {
            source_name,
            src_rate,
            src_ch,
            master,
            follow,
            sinks,
            outputs: Vec::new(),
            retiring: Vec::new(),
            next_id: 1,
            source_stats,
            source_stop,
            source_join: Some(source_join),
            global_stop,
        })
    }

    /// Add an output by device name (or part of one). The device does not have to be
    /// present yet: it is waited for and connected as soon as it appears.
    pub fn add_output(&mut self, needle: &str, delay_ms: u32, volume_pct: u32) -> Result<u64> {
        if let Some(addr) = needle.strip_prefix("net:") {
            return self.add_net_output(addr, delay_ms, volume_pct);
        }
        let host = cpal::default_host();
        // Prefer the device's full name so later matching is exact.
        let name = resolve_output_name(&host, needle).unwrap_or_else(|| needle.to_string());
        if self.outputs.iter().any(|o| o.name == name) {
            bail!("\"{name}\" is already in the group");
        }

        let rb = HeapRb::<f32>::new(self.src_rate as usize * self.src_ch * 2); // 2 s headroom
        let (prod, cons) = rb.split();
        let id = self.next_id;
        self.next_id += 1;

        let settings = Arc::new(Settings::new(delay_ms, volume_pct));
        let stats = Arc::new(Stats::default());
        let slave = Arc::new(Mutex::new(Slave::new(
            cons,
            self.src_ch,
            self.src_rate,
            settings.clone(),
            self.master.clone(),
            stats.clone(),
        )?));

        let sink = Arc::new(Sink { id, prod: Mutex::new(prod) });
        self.sinks.rcu(|cur| {
            let mut v = Vec::clone(cur);
            v.push(sink.clone());
            v
        });

        let stop = Arc::new(AtomicBool::new(false));
        let join = supervisor::spawn(
            name.clone(),
            {
                let (needle, source_name) = (name.clone(), self.source_name.clone());
                move || OutputConnector { needle, slave, source_name }
            },
            stop.clone(),
            stats.clone(),
            Timing::default(),
        );
        log(format!("added output: {name}"));
        self.outputs.push(OutputHandle { id, name, settings, stats, net: None, stop, join: Some(join) });
        Ok(id)
    }

    /// Add a network receiver (`host[:port]`). Like a local device it may be absent for
    /// now: the supervisor keeps trying, and reconnects if it drops later.
    fn add_net_output(&mut self, hostport: &str, delay_ms: u32, volume_pct: u32) -> Result<u64> {
        let addr = netout::parse_receiver_addr(hostport)?;
        let key = format!("net:{addr}");
        if self.outputs.iter().any(|o| o.name == key) {
            bail!("{addr} is already in the group");
        }
        let rb = HeapRb::<f32>::new(self.src_rate as usize * self.src_ch * 2); // 2 s headroom
        let (prod, cons) = rb.split();
        let id = self.next_id;
        self.next_id += 1;

        let settings = Arc::new(Settings::new(delay_ms, volume_pct));
        let stats = Arc::new(Stats::default());
        let net = Arc::new(NetStats::default());
        let sender = Arc::new(Mutex::new(NetSender::new(cons, self.src_ch)));

        let sink = Arc::new(Sink { id, prod: Mutex::new(prod) });
        self.sinks.rcu(|cur| {
            let mut v = Vec::clone(cur);
            v.push(sink.clone());
            v
        });

        let stop = Arc::new(AtomicBool::new(false));
        let join = supervisor::spawn(
            key.clone(),
            {
                let (settings, net) = (settings.clone(), net.clone());
                let (rate, channels) = (self.src_rate, self.src_ch as u16);
                move || NetConnector {
                    addr,
                    sender,
                    settings,
                    stats: net,
                    rate,
                    channels,
                    handshake_timeout: std::time::Duration::from_millis(1500),
                }
            },
            stop.clone(),
            stats.clone(),
            Timing::default(),
        );
        log(format!("added network output: {addr}"));
        self.outputs.push(OutputHandle { id, name: key, settings, stats, net: Some(net), stop, join: Some(join) });
        Ok(id)
    }

    /// Stop and forget an output. Returns immediately; the thread is reaped later.
    pub fn remove_output(&mut self, id: u64) {
        let Some(pos) = self.outputs.iter().position(|o| o.id == id) else { return };
        let mut h = self.outputs.remove(pos);
        h.stop.store(true, Ordering::Relaxed);
        self.sinks.rcu(|cur| cur.iter().filter(|s| s.id != id).cloned().collect::<Vec<_>>());
        if let Some(j) = h.join.take() {
            self.retiring.push(j);
        }
        log(format!("removed output: {}", h.name));
    }

    pub fn handles(&self) -> &[OutputHandle] {
        &self.outputs
    }

    pub fn source_connected(&self) -> bool {
        self.source_stats.connected.load(Ordering::Relaxed)
    }

    pub fn master_gain(&self) -> f32 {
        f32::from_bits(self.master.load(Ordering::Relaxed))
    }

    pub fn follow_volume(&self) -> bool {
        self.follow.load(Ordering::Relaxed)
    }

    pub fn set_follow_volume(&mut self, on: bool) {
        self.follow.store(on, Ordering::Relaxed);
    }

    /// Housekeeping: join threads of removed outputs once they have finished.
    pub fn tick(&mut self) {
        let mut i = 0;
        while i < self.retiring.len() {
            if self.retiring[i].is_finished() {
                let _ = self.retiring.swap_remove(i).join();
            } else {
                i += 1;
            }
        }
    }

    pub fn shutdown(&mut self) {
        for o in &self.outputs {
            o.stop.store(true, Ordering::Relaxed);
        }
        self.source_stop.store(true, Ordering::Relaxed);
        self.global_stop.store(true, Ordering::Relaxed);
        for o in &mut self.outputs {
            if let Some(j) = o.join.take() {
                let _ = j.join();
            }
        }
        for j in self.retiring.drain(..) {
            let _ = j.join();
        }
        if let Some(j) = self.source_join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}
