//! End-to-end tests of network streaming: a real broadcaster connector talking to a
//! real receiver over real UDP sockets on loopback. Only the sound card is replaced,
//! by a thread that pulls audio the way a device callback would.

use crate::netout::{NetConnector, NetSender, NetStats};
use crate::proto::{self, Packet, Status};
use crate::receiver::{self, ReceiverConfig, RxStatus, StartOutput};
use crate::slave::Settings;
use crate::supervisor::Connector;
use ringbuf::traits::{Producer, Split};
use ringbuf::HeapRb;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(6);
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// A receiver whose "sound card" records the peak level of every 10 ms it plays.
struct Rx {
    addr: SocketAddr,
    status: Arc<RxStatus>,
    settings: Arc<Settings>,
    peaks: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Rx {
    fn start(peer_timeout_ms: u64) -> Rx {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let status = Arc::new(RxStatus::default());
        let settings = Arc::new(Settings::new(0, 0));
        let peaks = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let p2 = peaks.clone();
        let start_output: StartOutput = Box::new(move |slave, out_stop, stats| {
            let peaks = p2.clone();
            thread::spawn(move || {
                stats.connected.store(true, Ordering::Relaxed);
                let mut buf = vec![0.0f32; 480 * 2]; // 10 ms of 48 kHz stereo
                while !out_stop.load(Ordering::Relaxed) {
                    slave.lock().unwrap().fill(&mut buf);
                    let pk = buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    peaks.lock().unwrap().push(pk);
                    thread::sleep(Duration::from_millis(10));
                }
                stats.connected.store(false, Ordering::Relaxed);
            })
        });

        let (st, se, sp) = (status.clone(), settings.clone(), stop.clone());
        let join = thread::spawn(move || {
            let cfg = ReceiverConfig {
                name: "Test Receiver".into(),
                jitter_ms: 20,
                peer_timeout: Duration::from_millis(peer_timeout_ms),
            };
            let master = Arc::new(AtomicU32::new(1.0f32.to_bits()));
            receiver::run(sock, cfg, master, se, start_output, st, sp).unwrap();
        });
        Rx { addr, status, settings, peaks, stop, join: Some(join) }
    }

    fn recent_peak(&self, n: usize) -> f32 {
        let p = self.peaks.lock().unwrap();
        p[p.len().saturating_sub(n)..].iter().fold(0.0f32, |m, s| m.max(*s))
    }
}

impl Drop for Rx {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// A broadcaster's view of one receiver, with a feeder thread that "captures" a sine.
struct Tx {
    connector: NetConnector,
    settings: Arc<Settings>,
    stats: Arc<NetStats>,
    feeder_stop: Arc<AtomicBool>,
    feeder: Option<JoinHandle<()>>,
}

impl Tx {
    fn new(rx: SocketAddr) -> Tx {
        let (mut prod, cons) = HeapRb::<f32>::new(48_000 * 2 * 2).split();
        let settings = Arc::new(Settings::new(0, 100));
        let stats = Arc::new(NetStats::default());
        let connector = NetConnector {
            addr: rx,
            sender: Arc::new(Mutex::new(NetSender::new(cons, 2))),
            settings: settings.clone(),
            stats: stats.clone(),
            rate: 48_000,
            channels: 2,
            handshake_timeout: Duration::from_millis(1500),
        };
        let feeder_stop = Arc::new(AtomicBool::new(false));
        let fs = feeder_stop.clone();
        let feeder = thread::spawn(move || {
            let mut phase = 0.0f32;
            let mut next = Instant::now();
            while !fs.load(Ordering::Relaxed) {
                let mut chunk = Vec::with_capacity(960);
                for _ in 0..480 {
                    let s = phase.sin() * 0.5;
                    phase += 0.1;
                    chunk.push(s);
                    chunk.push(s);
                }
                let _ = prod.push_slice(&chunk);
                next += Duration::from_millis(10);
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Tx { connector, settings, stats, feeder_stop, feeder: Some(feeder) }
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        self.feeder_stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.feeder.take() {
            let _ = j.join();
        }
    }
}

fn raw_client(bind: &str, to: SocketAddr) -> Option<UdpSocket> {
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(to).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(1000))).ok()?;
    Some(s)
}

fn hello(s: &UdpSocket, session: u32, rate: u32, ch: u16, frames: u16) -> Option<Status> {
    let mut b = Vec::new();
    proto::encode_hello(&mut b, session, rate, ch, frames, "raw");
    s.send(&b).unwrap();
    let mut rb = [0u8; 512];
    let n = s.recv(&mut rb).ok()?;
    match proto::decode(&rb[..n]) {
        Some(Packet::Ack { session: id, status, .. }) if id == session => Some(status),
        _ => None,
    }
}

#[test]
fn audio_and_every_control_flow_over_real_udp() {
    let rx = Rx::start(1500);
    let mut tx = Tx::new(rx.addr);
    let conn = tx.connector.connect().expect("connect to receiver");

    wait_until("session starts", || rx.status.active.load(Ordering::Relaxed));
    wait_until("audio is audible at the receiver", || rx.recent_peak(20) > 0.4);

    // volume set on the broadcaster changes what the receiver plays
    tx.settings.adjust_volume(-50);
    wait_until("volume halves", || (0.2..0.3).contains(&rx.recent_peak(20)));

    // mute and unmute
    tx.settings.toggle_mute();
    wait_until("muted", || rx.recent_peak(20) < 0.001);
    tx.settings.toggle_mute();
    wait_until("unmuted", || rx.recent_peak(20) > 0.2);

    // delay is carried across (its audible effect is covered by the slave tests)
    tx.settings.adjust_delay(100);
    wait_until("delay arrives", || rx.settings.delay_ms.load(Ordering::Relaxed) == 100);

    // the broadcaster learns how the receiver is doing
    wait_until("stats flow back", || tx.stats.received.load(Ordering::Relaxed) > 50);
    assert_eq!(tx.stats.lost.load(Ordering::Relaxed), 0, "loopback must not lose packets");
    let rtt = tx.stats.rtt_us.load(Ordering::Relaxed);
    assert!(rtt < 50_000, "rtt {rtt} us");
    assert!(tx.stats.playing.load(Ordering::Relaxed), "receiver reports a working output");
    assert!(tx.stats.packets_sent.load(Ordering::Relaxed) > 50);

    // a clean disconnect (BYE) frees the receiver at once
    drop(conn);
    wait_until("receiver sees BYE", || !rx.status.active.load(Ordering::Relaxed));

    // reconnecting starts a fresh session and audio resumes (volume 50%, delay 100 ms kept)
    let _conn2 = tx.connector.connect().expect("reconnect");
    wait_until("second session", || rx.status.sessions.load(Ordering::Relaxed) == 2);
    wait_until("audio resumes after reconnect", || rx.recent_peak(20) > 0.15);
    assert!(rx.recent_peak(20) < 0.3, "volume setting survived the reconnect");
}

#[test]
fn a_dead_receiver_is_noticed_and_a_new_one_takes_its_place() {
    let rx = Rx::start(1500);
    let mut tx = Tx::new(rx.addr);
    let mut conn = tx.connector.connect().expect("connect");
    assert!(tx.connector.is_alive(&mut conn));

    // the receiver process dies without saying goodbye
    drop(rx);
    wait_until("broadcaster notices the silence", || !tx.connector.is_alive(&mut conn));
    drop(conn);
    // and while it is gone, connecting fails with a clear message rather than hanging
    tx.connector.handshake_timeout = Duration::from_millis(400);
    let err = tx.connector.connect().err().expect("nobody home").to_string();
    assert!(err.contains("no answer") || err.contains("cannot reach"), "{err}");
}

#[test]
fn the_receiver_gives_up_on_a_silent_broadcaster() {
    let rx = Rx::start(300);
    let c = raw_client("127.0.0.1:0", rx.addr).unwrap();
    assert_eq!(hello(&c, 11, 48_000, 2, 150), Some(Status::Ok));
    wait_until("session active", || rx.status.active.load(Ordering::Relaxed));
    // the broadcaster vanishes (no BYE, no packets): the session must end by timeout
    wait_until("session times out", || !rx.status.active.load(Ordering::Relaxed));
    // and the receiver is free again for a new one
    assert_eq!(hello(&c, 12, 48_000, 2, 150), Some(Status::Ok));
    assert_eq!(rx.status.sessions.load(Ordering::Relaxed), 2);
}

#[test]
fn handshake_edge_cases() {
    let rx = Rx::start(5_000);
    let a = raw_client("127.0.0.1:0", rx.addr).unwrap();

    // impossible formats are refused and start nothing
    assert_eq!(hello(&a, 1, 48_000, 0, 150), Some(Status::Unsupported));
    assert_eq!(hello(&a, 2, 5, 2, 150), Some(Status::Unsupported));
    assert_eq!(hello(&a, 3, 48_000, 2, 9_000), Some(Status::Unsupported));
    assert_eq!(rx.status.sessions.load(Ordering::Relaxed), 0);

    // a good HELLO starts a session; a retransmitted HELLO (lost ACK) is answered, not duplicated
    assert_eq!(hello(&a, 20, 48_000, 2, 150), Some(Status::Ok));
    assert_eq!(hello(&a, 20, 48_000, 2, 150), Some(Status::Ok));
    assert_eq!(rx.status.sessions.load(Ordering::Relaxed), 1);

    // a restarted broadcaster on the same machine takes the session over
    let a2 = raw_client("127.0.0.1:0", rx.addr).unwrap();
    assert_eq!(hello(&a2, 21, 48_000, 2, 150), Some(Status::Ok));
    assert_eq!(rx.status.sessions.load(Ordering::Relaxed), 2);

    // a different machine is told the receiver is busy (needs a second loopback address)
    if let Some(other) = raw_client("127.0.0.2:0", rx.addr) {
        assert_eq!(hello(&other, 30, 48_000, 2, 150), Some(Status::Busy));
        assert_eq!(rx.status.sessions.load(Ordering::Relaxed), 2, "busy must not disturb the session");
    }

    // junk and packets for a stale session are ignored; the receiver stays up
    a2.send(b"definitely not audio").unwrap();
    let mut b = Vec::new();
    proto::encode_audio_start(&mut b, 999, 0, 1);
    proto::push_samples(&mut b, &[1.0, 1.0]);
    a2.send(&b).unwrap();
    proto::encode_bye(&mut b, 999); // BYE for someone else's session
    a2.send(&b).unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(rx.status.active.load(Ordering::Relaxed), "foreign BYE must not end our session");
}

#[test]
fn announcements_make_the_receiver_discoverable() {
    use crate::discovery::Discovery;
    let d = Discovery::start(0);
    let status = Arc::new(RxStatus::default());
    let stop = Arc::new(AtomicBool::new(false));
    let target: SocketAddr = format!("127.0.0.1:{}", d.port()).parse().unwrap();
    let h = receiver::spawn_announcer(target, 47_800, "Den".into(), status.clone(), stop.clone());
    wait_until("receiver discovered", || d.receivers().iter().any(|r| r.name == "Den" && !r.busy));
    status.active.store(true, Ordering::Relaxed);
    wait_until("busy flag follows the session", || d.receivers().iter().any(|r| r.name == "Den" && r.busy));
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
}
