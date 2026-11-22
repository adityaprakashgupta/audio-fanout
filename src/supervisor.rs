//! Connection supervision: keep one stream alive, notice when it dies, and bring it
//! back. The loop is generic over a [`Connector`] so it can be tested without audio
//! hardware.
//!
//! Each supervised stream gets its own thread, and the stream is created AND dropped
//! on that thread (cpal streams are not Send on every platform).

use crate::slave::Stats;
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub trait Connector {
    /// A live connection. Dropping it must stop the stream.
    type Conn;
    /// Find the device, configure, build and start the stream.
    fn connect(&mut self) -> Result<Self::Conn>;
    /// Is the connection still healthy? Called every `Timing::poll`.
    fn is_alive(&mut self, conn: &mut Self::Conn) -> bool;
    /// Human-readable summary for log lines ("Speakers (48000 Hz, 2 ch)").
    fn describe(&self, conn: &Self::Conn) -> String;
}

#[derive(Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            poll: Duration::from_millis(200),
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(5),
        }
    }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Connect, watch, and reconnect until `stop` is set. Retries use exponential
/// backoff (min -> max); a successful connection resets it. Log lines are printed
/// on state changes only, so a missing device doesn't spam the terminal.
pub fn run<C: Connector>(label: &str, mut c: C, stop: &AtomicBool, stats: &Stats, t: Timing) {
    let mut backoff = t.backoff_min;
    let mut last_err: Option<String> = None;
    let mut ever_connected = false;

    while !stop.load(Ordering::Relaxed) {
        match c.connect() {
            Ok(mut conn) => {
                let what = c.describe(&conn);
                if ever_connected {
                    crate::log::log(format!("[{label}] reconnected: {what}"));
                } else {
                    crate::log::log(format!("[{label}] connected: {what}"));
                }
                ever_connected = true;
                last_err = None;
                backoff = t.backoff_min;
                stats.connected.store(true, Ordering::Relaxed);

                while !stop.load(Ordering::Relaxed) && c.is_alive(&mut conn) {
                    thread::sleep(t.poll);
                }
                drop(conn); // stop the stream on this thread, before anything is rebuilt
                stats.connected.store(false, Ordering::Relaxed);
                if !stop.load(Ordering::Relaxed) {
                    stats.reconnects.fetch_add(1, Ordering::Relaxed);
                    crate::log::log(format!("[{label}] lost, waiting for it to come back..."));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if last_err.as_deref() != Some(msg.as_str()) {
                    crate::log::log(format!("[{label}] not available: {msg} (retrying)"));
                    last_err = Some(msg);
                }
                sleep_unless_stopped(backoff, stop);
                backoff = (backoff * 2).min(t.backoff_max);
            }
        }
    }
}

/// Run a supervisor on its own thread. `make` runs on that thread, so the connector
/// (and every stream it builds) never crosses threads.
pub fn spawn<C, F>(
    label: String,
    make: F,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
    t: Timing,
) -> JoinHandle<()>
where
    C: Connector,
    F: FnOnce() -> C + Send + 'static,
{
    thread::Builder::new()
        .name(format!("fanout-{label}"))
        .spawn(move || {
            let c = make();
            run(&label, c, &stop, &stats, t);
        })
        .expect("spawn supervisor thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::atomic::AtomicU32;

    /// A pretend device that can be plugged and unplugged from the test.
    struct Fake {
        present: Arc<AtomicBool>,
        attempts: Arc<AtomicU32>,
    }

    impl Connector for Fake {
        type Conn = ();
        fn connect(&mut self) -> Result<()> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            if self.present.load(Ordering::Relaxed) {
                Ok(())
            } else {
                Err(anyhow!("device not found"))
            }
        }
        fn is_alive(&mut self, _: &mut ()) -> bool {
            self.present.load(Ordering::Relaxed)
        }
        fn describe(&self, _: &()) -> String {
            "fake".into()
        }
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting for: {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn connects_late_survives_unplug_and_reconnects() {
        let present = Arc::new(AtomicBool::new(false)); // not plugged in at startup
        let attempts = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let timing = Timing {
            poll: Duration::from_millis(5),
            backoff_min: Duration::from_millis(5),
            backoff_max: Duration::from_millis(20),
        };
        let (p, a) = (present.clone(), attempts.clone());
        let h = spawn("fake".into(), move || Fake { present: p, attempts: a }, stop.clone(), stats.clone(), timing);

        // 1. Device absent at startup: supervisor keeps retrying, doesn't crash or exit.
        wait_for("retries while absent", || attempts.load(Ordering::Relaxed) >= 3);
        assert!(!stats.connected.load(Ordering::Relaxed));

        // 2. Device appears: connects without a restart.
        present.store(true, Ordering::Relaxed);
        wait_for("first connect", || stats.connected.load(Ordering::Relaxed));
        assert_eq!(stats.reconnects.load(Ordering::Relaxed), 0);

        // 3. Unplugged: noticed, counted.
        present.store(false, Ordering::Relaxed);
        wait_for("disconnect noticed", || !stats.connected.load(Ordering::Relaxed));
        assert_eq!(stats.reconnects.load(Ordering::Relaxed), 1);

        // 4. Plugged back in: reconnects.
        present.store(true, Ordering::Relaxed);
        wait_for("reconnect", || stats.connected.load(Ordering::Relaxed));

        // 5. Stop is honoured promptly while connected.
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn stop_interrupts_backoff() {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let timing = Timing {
            poll: Duration::from_millis(5),
            backoff_min: Duration::from_secs(30), // would hang the test if not interruptible
            backoff_max: Duration::from_secs(30),
        };
        let fake = Fake { present: Arc::new(AtomicBool::new(false)), attempts: Arc::new(AtomicU32::new(0)) };
        let s2 = stop.clone();
        let h = spawn("fake".into(), move || fake, s2, stats, timing);
        thread::sleep(Duration::from_millis(50));
        let t0 = Instant::now();
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(1), "stop took {:?}", t0.elapsed());
    }
}
