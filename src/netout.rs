//! Broadcaster side of network streaming: one [`NetConnector`] per receiver, supervised
//! exactly like a local output (connect, watch, reconnect with backoff).
//!
//! The broadcaster does no timing work: it forwards the audio it captures as it
//! arrives and sends the user's per-output settings (delay, volume, mute) to the
//! receiver, which applies them with the same playback pipeline a local output uses.
//! So every control behaves as it does for a local device.

use crate::proto::{self, Packet, Status};
use crate::slave::Settings;
use crate::supervisor::Connector;
use anyhow::{anyhow, bail, Result};
use ringbuf::traits::{Consumer, Observer};
use ringbuf::HeapCons;
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// No answer from the receiver for this long means it is gone.
const PEER_TIMEOUT: Duration = Duration::from_millis(1500);
const HEARTBEAT: Duration = Duration::from_millis(250);

/// What the receiver last told us about itself. Read by the UI.
#[derive(Default)]
pub struct NetStats {
    pub rtt_us: AtomicU32,
    pub remote_fill_ms: AtomicU32,
    pub remote_target_ms: AtomicU32,
    pub received: AtomicU32,
    pub lost: AtomicU32,
    pub underruns: AtomicU32,
    pub playing: AtomicBool, // the receiver has a working output device
    pub packets_sent: AtomicU64,
}

impl NetStats {
    pub fn loss_pct(&self) -> f32 {
        let (r, l) = (self.received.load(Ordering::Relaxed) as f32, self.lost.load(Ordering::Relaxed) as f32);
        if r + l == 0.0 {
            0.0
        } else {
            100.0 * l / (r + l)
        }
    }
}

/// Turns captured audio into packets. Pure logic: no sockets, so it is testable.
pub struct NetSender {
    cons: HeapCons<f32>,
    ch: usize,
    fpp: usize, // frames per packet
    session: u32,
    seq: u32,
    buf: Vec<u8>,
    scratch: Vec<f32>,
}

impl NetSender {
    pub fn new(cons: HeapCons<f32>, ch: usize) -> Self {
        let fpp = proto::packet_frames(ch);
        NetSender { cons, ch, fpp, session: 0, seq: 0, buf: Vec::with_capacity(1500), scratch: vec![0.0; fpp * ch] }
    }

    pub fn frames_per_packet(&self) -> usize {
        self.fpp
    }

    /// Begin a new session: audio buffered while nobody was listening is stale, drop it.
    pub fn start_session(&mut self, session: u32) {
        let n = self.cons.occupied_len();
        self.cons.skip(n);
        self.session = session;
        self.seq = 0;
    }

    /// Send every complete packet that is available. Returns how many were sent.
    pub fn pump(&mut self, mut send: impl FnMut(&[u8])) -> usize {
        let need = self.fpp * self.ch;
        let mut n = 0;
        while self.cons.occupied_len() >= need {
            self.cons.pop_slice(&mut self.scratch[..need]);
            proto::encode_audio_start(&mut self.buf, self.session, self.seq, self.fpp as u16);
            proto::push_samples(&mut self.buf, &self.scratch[..need]);
            send(&self.buf);
            self.seq = self.seq.wrapping_add(1);
            n += 1;
        }
        n
    }
}

/// `host` or `host:port` (default port 47800); IPv4 preferred.
pub fn parse_receiver_addr(text: &str) -> Result<SocketAddr> {
    let t = text.trim();
    if t.is_empty() {
        bail!("empty receiver address");
    }
    let with_port = if t.contains(':') { t.to_string() } else { format!("{t}:{}", proto::DEFAULT_PORT) };
    let all: Vec<SocketAddr> = with_port
        .to_socket_addrs()
        .map_err(|e| anyhow!("cannot resolve \"{t}\": {e}"))?
        .collect();
    all.iter().find(|a| a.is_ipv4()).or(all.first()).copied().ok_or_else(|| anyhow!("\"{t}\" has no address"))
}

pub fn local_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "audio-fanout".to_string())
}

pub fn new_session_id() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    (nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(2_654_435_761)).max(1)
}

pub struct NetConnector {
    pub addr: SocketAddr,
    pub sender: Arc<Mutex<NetSender>>,
    pub settings: Arc<Settings>,
    pub stats: Arc<NetStats>,
    pub rate: u32,
    pub channels: u16,
    pub handshake_timeout: Duration,
}

pub struct NetConn {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    desc: String,
}

impl Drop for NetConn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn is_transient(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset)
}

impl Connector for NetConnector {
    type Conn = NetConn;

    fn connect(&mut self) -> Result<NetConn> {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.connect(self.addr)?;
        sock.set_read_timeout(Some(Duration::from_millis(150)))?;
        let session = new_session_id();
        let fpp = self.sender.lock().map_err(|_| anyhow!("sender lock poisoned"))?.frames_per_packet();

        // Handshake: HELLO until ACK (UDP can lose either).
        let (mut out, mut rb) = (Vec::new(), [0u8; 512]);
        let deadline = Instant::now() + self.handshake_timeout;
        let peer_name = loop {
            proto::encode_hello(&mut out, session, self.rate, self.channels, fpp as u16, &local_name());
            match sock.send(&out) {
                Ok(_) => {}
                Err(e) if is_transient(&e) => {}
                Err(e) => bail!("cannot reach {}: {e}", self.addr),
            }
            match sock.recv(&mut rb) {
                Ok(n) => {
                    if let Some(Packet::Ack { session: s, status, name }) = proto::decode(&rb[..n]) {
                        if s == session {
                            match status {
                                Status::Ok => break name.to_string(),
                                Status::Busy => bail!("receiver is busy with another broadcaster"),
                                Status::Unsupported => bail!("receiver does not support this audio format"),
                            }
                        }
                    }
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => bail!("cannot reach {}: {e}", self.addr),
            }
            if Instant::now() >= deadline {
                bail!("no answer from {} (is the receiver running? firewall?)", self.addr);
            }
        };

        self.sender.lock().map_err(|_| anyhow!("sender lock poisoned"))?.start_session(session);
        let (stop, alive) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(true)));
        let join = {
            let (sender, settings, stats) = (self.sender.clone(), self.settings.clone(), self.stats.clone());
            let (stop, alive) = (stop.clone(), alive.clone());
            thread::Builder::new()
                .name(format!("fanout-net-{}", self.addr))
                .spawn(move || send_loop(sock, session, sender, settings, stats, stop, alive))
                .map_err(|e| anyhow!("cannot start sender thread: {e}"))?
        };
        Ok(NetConn {
            stop,
            alive,
            join: Some(join),
            desc: format!("{peer_name} at {} ({} Hz, {} ch)", self.addr, self.rate, self.channels),
        })
    }

    fn is_alive(&mut self, c: &mut NetConn) -> bool {
        c.alive.load(Ordering::Relaxed)
    }

    fn describe(&self, c: &NetConn) -> String {
        c.desc.clone()
    }
}

/// Forward audio, send settings the moment they change (and as a heartbeat), and read
/// the receiver's PONGs. Polls every millisecond; see the note on timer resolution in
/// `main` (Windows).
fn send_loop(
    sock: UdpSocket,
    session: u32,
    sender: Arc<Mutex<NetSender>>,
    settings: Arc<Settings>,
    stats: Arc<NetStats>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
) {
    let _ = sock.set_nonblocking(true);
    let start = Instant::now();
    let (mut out, mut rb) = (Vec::with_capacity(64), [0u8; 512]);
    let mut last_sent: Option<(u16, u8, bool)> = None;
    let mut last_state_at = Instant::now();
    let mut state_seq = 0u32;
    let mut last_pong = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if let Ok(mut s) = sender.lock() {
            let n = s.pump(|pkt| {
                let _ = sock.send(pkt); // UDP: a failed send is just a lost packet
            });
            stats.packets_sent.fetch_add(n as u64, Ordering::Relaxed);
        }

        let cur = (
            settings.delay_ms.load(Ordering::Relaxed) as u16,
            settings.volume_pct.load(Ordering::Relaxed) as u8,
            settings.muted.load(Ordering::Relaxed),
        );
        if last_sent != Some(cur) || last_state_at.elapsed() >= HEARTBEAT {
            proto::encode_state(&mut out, session, state_seq, cur.0, cur.1, cur.2, start.elapsed().as_micros() as u64);
            let _ = sock.send(&out);
            state_seq = state_seq.wrapping_add(1);
            last_sent = Some(cur);
            last_state_at = Instant::now();
        }

        while let Ok(n) = sock.recv(&mut rb) {
            if let Some(Packet::Pong { session: s, t_us, fill_ms, target_ms, received, lost, underruns, playing }) =
                proto::decode(&rb[..n])
            {
                if s == session {
                    last_pong = Instant::now();
                    let rtt = (start.elapsed().as_micros() as u64).saturating_sub(t_us);
                    stats.rtt_us.store(rtt.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                    stats.remote_fill_ms.store(fill_ms as u32, Ordering::Relaxed);
                    stats.remote_target_ms.store(target_ms as u32, Ordering::Relaxed);
                    stats.received.store(received, Ordering::Relaxed);
                    stats.lost.store(lost, Ordering::Relaxed);
                    stats.underruns.store(underruns, Ordering::Relaxed);
                    stats.playing.store(playing, Ordering::Relaxed);
                }
            }
        }

        if last_pong.elapsed() > PEER_TIMEOUT {
            break; // receiver gone: let the supervisor reconnect
        }
        thread::sleep(Duration::from_millis(1));
    }

    proto::encode_bye(&mut out, session);
    let _ = sock.send(&out);
    let _ = sock.send(&out); // twice: UDP
    alive.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;

    #[test]
    fn pump_packetizes_exactly_and_keeps_the_remainder() {
        let rb = HeapRb::<f32>::new(48_000 * 2);
        let (mut prod, cons) = rb.split();
        let mut tx = NetSender::new(cons, 2);
        assert_eq!(tx.frames_per_packet(), 150);
        tx.start_session(77);

        // 1000 frames of a ramp: 6 whole packets (900 frames), 100 left over
        let samples: Vec<f32> = (0..2000).map(|i| i as f32 / 2000.0).collect();
        prod.push_slice(&samples);
        let mut packets: Vec<Vec<u8>> = Vec::new();
        let sent = tx.pump(|p| packets.push(p.to_vec()));
        assert_eq!(sent, 6);
        for (i, p) in packets.iter().enumerate() {
            match proto::decode(p) {
                Some(Packet::Audio { session: 77, seq, frames: 150, data }) => {
                    assert_eq!(seq, i as u32);
                    let got: Vec<f32> = data.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
                    assert_eq!(got, &samples[i * 300..(i + 1) * 300]);
                }
                other => panic!("{other:?}"),
            }
        }
        // the remainder waits for more audio, then goes out in order
        assert_eq!(tx.pump(|_| panic!("nothing complete yet")), 0);
        prod.push_slice(&samples[..100]); // 50 more frames: 100 + 50 = one full packet
        let mut next = Vec::new();
        assert_eq!(tx.pump(|p| next.push(p.to_vec())), 1);
        assert!(matches!(proto::decode(&next[0]), Some(Packet::Audio { seq: 6, .. })));
    }

    #[test]
    fn a_new_session_drops_stale_audio() {
        let rb = HeapRb::<f32>::new(48_000 * 2);
        let (mut prod, cons) = rb.split();
        let mut tx = NetSender::new(cons, 2);
        prod.push_slice(&vec![0.5f32; 10_000]); // piled up while the receiver was away
        tx.start_session(1);
        assert_eq!(tx.pump(|_| panic!("stale audio must not be sent")), 0);
    }

    #[test]
    fn connecting_to_nothing_fails_clearly_and_quickly() {
        let rb = HeapRb::<f32>::new(1024);
        let (_prod, cons) = rb.split();
        let mut c = NetConnector {
            addr: "127.0.0.1:9".parse().unwrap(), // discard port: nobody speaking our protocol
            sender: Arc::new(Mutex::new(NetSender::new(cons, 2))),
            settings: Arc::new(Settings::new(0, 50)),
            stats: Arc::new(NetStats::default()),
            rate: 48_000,
            channels: 2,
            handshake_timeout: Duration::from_millis(400),
        };
        let t0 = Instant::now();
        let err = c.connect().err().expect("must fail").to_string();
        assert!(err.contains("no answer"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn receiver_addresses_parse_with_a_default_port() {
        assert_eq!(parse_receiver_addr("192.168.1.5").unwrap(), "192.168.1.5:47800".parse().unwrap());
        assert_eq!(parse_receiver_addr(" 10.0.0.2:5000 ").unwrap(), "10.0.0.2:5000".parse().unwrap());
        assert_eq!(parse_receiver_addr("127.0.0.1").unwrap().port(), 47_800);
        assert!(parse_receiver_addr("").is_err());
        assert!(parse_receiver_addr("   ").is_err());
        assert!(parse_receiver_addr("192.168.1.5:notaport").is_err());
        assert!(parse_receiver_addr("host with spaces").is_err());
    }

    #[test]
    fn loss_percentage() {
        let s = NetStats::default();
        assert_eq!(s.loss_pct(), 0.0);
        s.received.store(990, Ordering::Relaxed);
        s.lost.store(10, Ordering::Relaxed);
        assert!((s.loss_pct() - 1.0).abs() < 1e-4);
    }
}
