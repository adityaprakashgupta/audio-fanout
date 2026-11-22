//! Receiver side: a "network speaker". Listens for a broadcaster, turns its packets
//! into audio, and plays it on a local device through the same drift-correcting
//! pipeline as every other output. All controls (delay, volume, mute) come from the
//! broadcaster; the receiver's own system volume is applied on top.
//!
//! One broadcaster at a time. A broadcaster that restarts (same IP) takes over the
//! session; one from another IP is told the receiver is busy.

use crate::connectors::OutputConnector;
use crate::log::log;
use crate::netout::local_name;
use crate::proto::{self, Packet, Status};
use crate::slave::{Settings, Slave, Stats};
use crate::supervisor::{self, Timing};
use crate::volume;
use anyhow::Result;
use ringbuf::traits::{Observer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Short gaps are filled with silence so the timeline (and therefore every output's
/// delay) stays put. Longer ones are left to the playback pipeline to re-prime.
pub const MAX_FILL_PACKETS: u32 = 20;

/// Turns packets into ring-buffer samples and keeps the loss books. Pure logic.
pub struct Ingest {
    ch: usize,
    expected: Option<u32>,
    scratch: Vec<f32>,
    pub received: u32,
    pub lost: u32,
    pub late: u32,
}

impl Ingest {
    pub fn new(ch: usize) -> Self {
        Ingest { ch, expected: None, scratch: Vec::with_capacity(2048), received: 0, lost: 0, late: 0 }
    }

    pub fn on_audio(&mut self, seq: u32, frames: u16, data: &[u8], prod: &mut (impl Producer<Item = f32> + Observer)) {
        let n = frames as usize * self.ch;
        if frames == 0 || data.len() != n * 4 {
            return; // does not match the session's format
        }
        let exp = *self.expected.get_or_insert(seq);
        let diff = seq.wrapping_sub(exp) as i32; // wrap-safe ordering
        if diff < 0 {
            self.late += 1; // duplicate or arrived after its slot was already skipped
            return;
        }
        if diff > 0 {
            let missing = diff as u32;
            self.lost = self.lost.saturating_add(missing);
            if missing <= MAX_FILL_PACKETS {
                let silence = missing as usize * n;
                if prod.vacant_len() >= silence + n {
                    prod.push_iter(std::iter::repeat(0.0f32).take(silence));
                }
            }
        }
        self.expected = Some(seq.wrapping_add(1));
        if prod.vacant_len() < n {
            return; // consumer stalled (e.g. output device unplugged): drop, don't count as loss
        }
        self.scratch.clear();
        self.scratch.extend(data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
        prod.push_slice(&self.scratch);
        self.received += 1;
    }
}

/// Shared with the announcer and, in tests, the outside world.
#[derive(Default)]
pub struct RxStatus {
    pub active: AtomicBool,
    pub sessions: AtomicU32,
}

pub struct ReceiverConfig {
    pub name: String,
    /// Jitter cushion of the playback pipeline, on top of any delay the broadcaster sets.
    pub jitter_ms: u32,
    /// Silence from the broadcaster for this long ends the session.
    pub peer_timeout: Duration,
}

/// Starts playback for a session: given the pipeline, a stop flag and a stats block,
/// returns the thread that plays it. Real receivers run a supervised cpal output.
pub type StartOutput = Box<dyn FnMut(Arc<Mutex<Slave>>, Arc<AtomicBool>, Arc<Stats>) -> JoinHandle<()> + Send>;

struct Session {
    id: u32,
    peer: SocketAddr,
    prod: HeapProd<f32>,
    ingest: Ingest,
    stats: Arc<Stats>,
    rate: u32,
    out_stop: Arc<AtomicBool>,
    out_join: Option<JoinHandle<()>>,
    last_rx: Instant,
}

impl Session {
    fn end(mut self) {
        self.out_stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.out_join.take() {
            let _ = j.join();
        }
    }
}

fn valid_format(rate: u32, channels: u16, frames: u16) -> bool {
    (8_000..=192_000).contains(&rate)
        && (1..=8).contains(&channels)
        && frames >= 1
        && frames as usize * channels as usize * 4 <= 1400
}

fn send_ack(sock: &UdpSocket, to: SocketAddr, session: u32, status: Status, name: &str) {
    let mut b = Vec::with_capacity(64);
    proto::encode_ack(&mut b, session, status, name);
    let _ = sock.send_to(&b, to);
}

/// The receive loop. Runs until `stop`.
pub fn run(
    sock: UdpSocket,
    cfg: ReceiverConfig,
    master: Arc<AtomicU32>,
    settings: Arc<Settings>,
    mut start_output: StartOutput,
    status: Arc<RxStatus>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    let (mut buf, mut out) = (vec![0u8; 2048], Vec::with_capacity(64));
    let mut session: Option<Session> = None;
    let mut last_report = Instant::now();

    let begin = |from: SocketAddr,
                 id: u32,
                 rate: u32,
                 ch: usize,
                 start_output: &mut StartOutput|
     -> Result<Session> {
        let rb = HeapRb::<f32>::new(rate as usize * ch * 2); // 2 s headroom
        let (prod, cons) = rb.split();
        let stats = Arc::new(Stats::default());
        settings.apply_remote(0, 0, false); // silent until the broadcaster's first STATE (a few ms)
        let slave = Slave::new(cons, ch, rate, settings.clone(), master.clone(), stats.clone())?
            .with_cushion_ms(cfg.jitter_ms);
        let out_stop = Arc::new(AtomicBool::new(false));
        let join = start_output(Arc::new(Mutex::new(slave)), out_stop.clone(), stats.clone());
        Ok(Session {
            id,
            peer: from,
            prod,
            ingest: Ingest::new(ch),
            stats,
            rate,
            out_stop,
            out_join: Some(join),
            last_rx: Instant::now(),
        })
    };

    while !stop.load(Ordering::Relaxed) {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            match proto::decode(&buf[..n]) {
                Some(Packet::Hello { session: id, rate, channels, frames, .. }) => {
                    let same_peer_retry = session.as_ref().map_or(false, |s| s.id == id && s.peer == from);
                    let same_host_restart =
                        session.as_ref().map_or(false, |s| s.peer.ip() == from.ip() && s.id != id);
                    if same_peer_retry {
                        send_ack(&sock, from, id, Status::Ok, &cfg.name); // our ACK was lost
                    } else if session.is_some() && !same_host_restart {
                        send_ack(&sock, from, id, Status::Busy, &cfg.name);
                    } else if !valid_format(rate, channels, frames) {
                        send_ack(&sock, from, id, Status::Unsupported, &cfg.name);
                    } else {
                        if let Some(old) = session.take() {
                            log("[receiver] broadcaster restarted: replacing its session");
                            old.end();
                        }
                        match begin(from, id, rate, channels as usize, &mut start_output) {
                            Ok(s) => {
                                log(format!("[receiver] streaming from {from} ({rate} Hz, {channels} ch)"));
                                session = Some(s);
                                status.active.store(true, Ordering::Relaxed);
                                status.sessions.fetch_add(1, Ordering::Relaxed);
                                send_ack(&sock, from, id, Status::Ok, &cfg.name);
                            }
                            Err(e) => {
                                log(format!("[receiver] cannot start session: {e}"));
                                send_ack(&sock, from, id, Status::Unsupported, &cfg.name);
                            }
                        }
                    }
                }
                Some(Packet::Audio { session: id, seq, frames, data }) => {
                    if let Some(s) = session.as_mut().filter(|s| s.id == id && s.peer == from) {
                        s.last_rx = Instant::now();
                        s.ingest.on_audio(seq, frames, data, &mut s.prod);
                    }
                }
                Some(Packet::State { session: id, delay_ms, volume_pct, muted, t_us, .. }) => {
                    if let Some(s) = session.as_mut().filter(|s| s.id == id && s.peer == from) {
                        s.last_rx = Instant::now();
                        settings.apply_remote(delay_ms as u32, volume_pct as u32, muted);
                        let rate = s.rate.max(1);
                        proto::encode_pong(
                            &mut out,
                            id,
                            t_us,
                            (s.stats.fill_frames.load(Ordering::Relaxed) as u64 * 1000 / rate as u64) as u16,
                            (s.stats.target_frames.load(Ordering::Relaxed) as u64 * 1000 / rate as u64) as u16,
                            s.ingest.received,
                            s.ingest.lost,
                            s.stats.underruns.load(Ordering::Relaxed),
                            s.stats.connected.load(Ordering::Relaxed),
                        );
                        let _ = sock.send_to(&out, from);
                    }
                }
                Some(Packet::Bye { session: id }) => {
                    if session.as_ref().map_or(false, |s| s.id == id && s.peer == from) {
                        log("[receiver] broadcaster disconnected");
                        if let Some(s) = session.take() {
                            s.end();
                        }
                        status.active.store(false, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }

        if session.as_ref().map_or(false, |s| s.last_rx.elapsed() > cfg.peer_timeout) {
            log("[receiver] lost the broadcaster (no packets): waiting for it to return");
            if let Some(s) = session.take() {
                s.end();
            }
            status.active.store(false, Ordering::Relaxed);
        }

        if let Some(s) = &session {
            if last_report.elapsed() >= Duration::from_secs(5) {
                last_report = Instant::now();
                let rate = s.rate.max(1) as u64;
                log(format!(
                    "[receiver] buffer {}/{} ms, packets {} received / {} lost / {} late, underruns {}",
                    s.stats.fill_frames.load(Ordering::Relaxed) as u64 * 1000 / rate,
                    s.stats.target_frames.load(Ordering::Relaxed) as u64 * 1000 / rate,
                    s.ingest.received,
                    s.ingest.lost,
                    s.ingest.late,
                    s.stats.underruns.load(Ordering::Relaxed)
                ));
            }
        }
    }

    if let Some(s) = session.take() {
        s.end();
    }
    status.active.store(false, Ordering::Relaxed);
    Ok(())
}

/// Tell the LAN we exist, once a second.
pub fn spawn_announcer(
    target: SocketAddr,
    port: u16,
    name: String,
    status: Arc<RxStatus>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("fanout-announce".into())
        .spawn(move || {
            let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
            let _ = sock.set_broadcast(true);
            let mut b = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                proto::encode_announce(&mut b, port, status.active.load(Ordering::Relaxed), &name);
                let _ = sock.send_to(&b, target);
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("spawn announcer")
}

pub struct ReceiveOptions {
    pub port: u16,
    pub name: String,
    /// Output device name (part of it); empty = the system default output.
    pub device: String,
    pub jitter_ms: u32,
    pub follow_volume: bool,
}

/// `audio-fanout --receive`: run until `stop`.
pub fn run_receiver(o: ReceiveOptions, stop: Arc<AtomicBool>) -> Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", o.port))
        .map_err(|e| anyhow::anyhow!("cannot listen on UDP port {}: {e} (already running?)", o.port))?;
    let port = sock.local_addr()?.port();
    let status = Arc::new(RxStatus::default());
    let name = if o.name.is_empty() { local_name() } else { o.name.clone() };

    let target: SocketAddr = ([255, 255, 255, 255], proto::DISCOVERY_PORT).into();
    let announcer = spawn_announcer(target, port, name.clone(), status.clone(), stop.clone());

    let follow = Arc::new(AtomicBool::new(o.follow_volume));
    let master = volume::start(stop.clone(), follow);
    let settings = Arc::new(Settings::new(0, 0));

    let device = o.device.clone();
    let start_output: StartOutput = Box::new(move |slave, out_stop, stats| {
        let device = device.clone();
        supervisor::spawn(
            "output".into(),
            move || OutputConnector { needle: device, slave, source_name: String::new() },
            out_stop,
            stats,
            Timing::default(),
        )
    });

    log(format!(
        "[receiver] \"{name}\" listening on UDP {port}; playing on {}",
        if o.device.is_empty() { "the default output device" } else { &o.device }
    ));
    let cfg = ReceiverConfig { name, jitter_ms: o.jitter_ms, peer_timeout: Duration::from_millis(1500) };
    let result = run(sock, cfg, master, settings, start_output, status, stop.clone());
    stop.store(true, Ordering::Relaxed);
    let _ = announcer.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::Consumer;

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn rig(ch: usize) -> (Ingest, HeapProd<f32>, ringbuf::HeapCons<f32>) {
        let (p, c) = HeapRb::<f32>::new(48_000 * ch).split();
        (Ingest::new(ch), p, c)
    }

    fn drain(c: &mut ringbuf::HeapCons<f32>) -> Vec<f32> {
        let mut v = vec![0.0; c.occupied_len()];
        c.pop_slice(&mut v);
        v
    }

    #[test]
    fn in_order_packets_pass_through() {
        let (mut ing, mut p, mut c) = rig(2);
        ing.on_audio(0, 2, &bytes(&[1.0, 2.0, 3.0, 4.0]), &mut p);
        ing.on_audio(1, 2, &bytes(&[5.0, 6.0, 7.0, 8.0]), &mut p);
        assert_eq!(drain(&mut c), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!((ing.received, ing.lost, ing.late), (2, 0, 0));
    }

    /// A lost packet becomes silence of the same length, so later audio stays on its
    /// slot of the timeline and the delay does not drift.
    #[test]
    fn a_gap_is_filled_with_silence_of_the_same_length() {
        let (mut ing, mut p, mut c) = rig(2);
        ing.on_audio(0, 2, &bytes(&[1.0; 4]), &mut p);
        ing.on_audio(3, 2, &bytes(&[9.0; 4]), &mut p); // 1 and 2 lost
        let got = drain(&mut c);
        assert_eq!(got.len(), 4 + 8 + 4);
        assert_eq!(&got[4..12], &[0.0; 8]);
        assert_eq!(&got[12..], &[9.0; 4]);
        assert_eq!((ing.received, ing.lost), (2, 2));
    }

    #[test]
    fn late_and_duplicate_packets_are_dropped() {
        let (mut ing, mut p, mut c) = rig(1);
        ing.on_audio(5, 2, &bytes(&[1.0, 1.0]), &mut p);
        ing.on_audio(6, 2, &bytes(&[2.0, 2.0]), &mut p);
        ing.on_audio(6, 2, &bytes(&[2.0, 2.0]), &mut p); // duplicate
        ing.on_audio(4, 2, &bytes(&[7.0, 7.0]), &mut p); // late
        assert_eq!(drain(&mut c), vec![1.0, 1.0, 2.0, 2.0]);
        assert_eq!((ing.received, ing.lost, ing.late), (2, 0, 2));
    }

    #[test]
    fn sequence_numbers_wrap_around() {
        let (mut ing, mut p, mut c) = rig(1);
        ing.on_audio(u32::MAX - 1, 1, &bytes(&[1.0]), &mut p);
        ing.on_audio(u32::MAX, 1, &bytes(&[2.0]), &mut p);
        ing.on_audio(0, 1, &bytes(&[3.0]), &mut p);
        ing.on_audio(1, 1, &bytes(&[4.0]), &mut p);
        assert_eq!(drain(&mut c), vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!((ing.received, ing.lost, ing.late), (4, 0, 0));
    }

    #[test]
    fn a_huge_gap_does_not_flood_the_buffer_with_silence() {
        let (mut ing, mut p, mut c) = rig(1);
        ing.on_audio(0, 1, &bytes(&[1.0]), &mut p);
        ing.on_audio(10_000, 1, &bytes(&[2.0]), &mut p);
        assert_eq!(drain(&mut c), vec![1.0, 2.0], "left to the pipeline to re-prime");
        assert_eq!(ing.lost, 9_999);
    }

    #[test]
    fn malformed_packets_and_full_buffers_are_survived() {
        let (mut ing, mut p, mut c) = rig(2);
        ing.on_audio(0, 2, &bytes(&[1.0; 3]), &mut p); // wrong length for 2 frames x 2 ch
        ing.on_audio(0, 0, &[], &mut p); // zero frames
        assert!(drain(&mut c).is_empty());
        // consumer stalled: the buffer fills, extra packets are dropped quietly
        let chunk = bytes(&vec![0.5f32; 2 * 100]);
        for seq in 0..2_000u32 {
            ing.on_audio(seq, 100, &chunk, &mut p);
        }
        assert!(c.occupied_len() <= 48_000 * 2);
        assert_eq!(ing.lost, 0, "dropping into a full buffer is not network loss");
    }

    #[test]
    fn format_validation() {
        assert!(valid_format(48_000, 2, 150));
        assert!(!valid_format(48_000, 0, 150));
        assert!(!valid_format(48_000, 9, 10));
        assert!(!valid_format(1, 2, 150));
        assert!(!valid_format(48_000, 2, 0));
        assert!(!valid_format(48_000, 2, 5000), "bigger than a safe datagram");
    }
}
