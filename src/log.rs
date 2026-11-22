//! Tiny in-memory log. Supervisors and connectors log here instead of printing, so
//! the TUI can show messages without them tearing the screen. In headless mode the
//! messages are also echoed to stdout.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const CAP: usize = 500;

static LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static ECHO: AtomicBool = AtomicBool::new(true);
static START: OnceLock<Instant> = OnceLock::new();

pub fn set_echo(on: bool) {
    ECHO.store(on, Ordering::Relaxed);
}

pub fn log(msg: impl AsRef<str>) {
    let secs = START.get_or_init(Instant::now).elapsed().as_secs();
    let line = format!("[{:02}:{:02}:{:02}] {}", secs / 3600, secs / 60 % 60, secs % 60, msg.as_ref());
    if ECHO.load(Ordering::Relaxed) {
        println!("{line}");
    }
    if let Ok(mut q) = LINES.lock() {
        if q.len() >= CAP {
            q.pop_front();
        }
        q.push_back(line);
    }
}

/// The last `n` lines, oldest first.
pub fn recent(n: usize) -> Vec<String> {
    match LINES.lock() {
        Ok(q) => q.iter().skip(q.len().saturating_sub(n)).cloned().collect(),
        Err(_) => Vec::new(),
    }
}
