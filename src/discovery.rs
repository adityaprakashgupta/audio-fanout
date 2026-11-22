//! Finding receivers on the LAN. Receivers broadcast a small ANNOUNCE every second;
//! the broadcaster listens and keeps a live list. A receiver that stops announcing
//! (laptop closed, program quit) drops out of the list a few seconds later.
//!
//! Networks that block broadcast (some guest Wi-Fi, "client isolation") will not show
//! receivers here; add them by address instead (`n` in the TUI, or `--send HOST`).

use crate::log::log;
use crate::proto::{self, Packet};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const EXPIRE: Duration = Duration::from_millis(3500);

#[derive(Clone, Debug, PartialEq)]
pub struct Receiver {
    pub name: String,
    pub addr: SocketAddr,
    pub busy: bool,
}

type Map = HashMap<SocketAddr, (String, bool, Instant)>;

pub struct Discovery {
    map: Arc<Mutex<Map>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    #[allow(dead_code)] // read by tests (ephemeral port)
    port: u16,
}

impl Discovery {
    /// Listen for announcements on `port` (0 = pick one; see [`Discovery::port`]).
    /// If the port is taken the list simply stays empty.
    pub fn start(port: u16) -> Discovery {
        let map: Arc<Mutex<Map>> = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let sock = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => s,
            Err(e) => {
                log(format!("discovery unavailable (port {port}: {e}); add receivers by address"));
                return Discovery { map, stop, join: None, port };
            }
        };
        let port = sock.local_addr().map(|a| a.port()).unwrap_or(port);
        let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
        let (m2, s2) = (map.clone(), stop.clone());
        let join = thread::Builder::new()
            .name("fanout-discovery".into())
            .spawn(move || {
                let mut buf = [0u8; 512];
                while !s2.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = sock.recv_from(&mut buf) {
                        if let Some(Packet::Announce { port, busy, name }) = proto::decode(&buf[..n]) {
                            let addr = SocketAddr::new(from.ip(), port);
                            if let Ok(mut g) = m2.lock() {
                                g.insert(addr, (name.to_string(), busy, Instant::now()));
                            }
                        }
                    }
                    if let Ok(mut g) = m2.lock() {
                        g.retain(|_, (_, _, seen)| seen.elapsed() < EXPIRE);
                    }
                }
            })
            .ok();
        Discovery { map, stop, join, port }
    }

    #[allow(dead_code)] // tests bind port 0 and need to know which one they got
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Receivers heard from recently, sorted by name.
    pub fn receivers(&self) -> Vec<Receiver> {
        let mut v: Vec<Receiver> = match self.map.lock() {
            Ok(g) => g
                .iter()
                .filter(|(_, (_, _, seen))| seen.elapsed() < EXPIRE)
                .map(|(addr, (name, busy, _))| Receiver { name: name.clone(), addr: *addr, busy: *busy })
                .collect(),
            Err(_) => Vec::new(),
        };
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.to_string().cmp(&b.addr.to_string())));
        v
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receivers_appear_when_announcing_and_vanish_when_they_stop() {
        let d = Discovery::start(0);
        assert!(d.port() != 0);
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target: SocketAddr = format!("127.0.0.1:{}", d.port()).parse().unwrap();
        let mut b = Vec::new();
        proto::encode_announce(&mut b, 47_800, false, "Kitchen");
        tx.send_to(&b, target).unwrap();
        proto::encode_announce(&mut b, 47_810, true, "Bedroom");
        tx.send_to(&b, target).unwrap();
        tx.send_to(b"garbage", target).unwrap(); // must be ignored

        let end = Instant::now() + Duration::from_secs(2);
        while d.receivers().len() < 2 {
            assert!(Instant::now() < end, "never discovered: {:?}", d.receivers());
            thread::sleep(Duration::from_millis(20));
        }
        let r = d.receivers();
        assert_eq!(r[0].name, "Bedroom");
        assert!(r[0].busy);
        assert_eq!(r[1].name, "Kitchen");
        assert_eq!(r[1].addr, "127.0.0.1:47800".parse().unwrap());

        // they stop announcing: gone after the expiry window
        let end = Instant::now() + Duration::from_secs(6);
        while !d.receivers().is_empty() {
            assert!(Instant::now() < end, "receivers never expired");
            thread::sleep(Duration::from_millis(100));
        }
    }
}
