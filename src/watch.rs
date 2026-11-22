//! Background device watcher: polls the OS device lists once a second so the UI can
//! show devices appearing and disappearing without ever blocking on enumeration.

use crate::devices::name_of;
use crate::discovery::{Discovery, Receiver};
use crate::proto::DISCOVERY_PORT;
use cpal::traits::HostTrait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Snapshot {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Network receivers announcing themselves on the LAN right now.
    pub receivers: Vec<Receiver>,
}

fn names(it: Result<impl Iterator<Item = cpal::Device>, impl std::fmt::Debug>) -> Vec<String> {
    let mut v: Vec<String> = match it {
        Ok(it) => it.map(|d| name_of(&d)).collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v.dedup();
    v
}

pub fn enumerate() -> Snapshot {
    let host = cpal::default_host();
    Snapshot { inputs: names(host.input_devices()), outputs: names(host.output_devices()), receivers: Vec::new() }
}

pub struct Watcher {
    snap: Arc<Mutex<Snapshot>>,
    discovery: Discovery,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn start() -> Watcher {
        let snap = Arc::new(Mutex::new(enumerate())); // ready immediately
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2) = (snap.clone(), stop.clone());
        let join = thread::Builder::new()
            .name("fanout-watch".into())
            .spawn(move || {
                while !st2.load(Ordering::Relaxed) {
                    for _ in 0..10 {
                        if st2.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    let fresh = enumerate();
                    if let Ok(mut g) = s2.lock() {
                        *g = fresh;
                    }
                }
            })
            .expect("spawn watcher");
        Watcher { snap, discovery: Discovery::start(DISCOVERY_PORT), stop, join: Some(join) }
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut s = self.snap.lock().map(|g| g.clone()).unwrap_or_default();
        s.receivers = self.discovery.receivers();
        s
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
