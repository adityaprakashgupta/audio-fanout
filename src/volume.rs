//! Follow the OS's default-output volume/mute and publish it as a linear gain.
//!
//! "System volume" = the volume of the default output device, i.e. what your
//! keyboard volume keys / tray slider control. When a virtual cable or BlackHole
//! is the default output, that is the virtual device's volume.
//!
//! Runs on its own thread (never the audio threads) and polls ~4x per second.
//!   Windows: IAudioEndpointVolume (exact dB -> linear)
//!   Linux  : `pactl` (PulseAudio and PipeWire-pulse); percent -> cubic, as Pulse does
//!   macOS  : `osascript` (approximate curve; returns nothing for devices without volume)

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Shared gain as f32 bits. Starts at 1.0 and stays there if the OS value can't be read.
pub fn start(stop: Arc<AtomicBool>) -> Arc<AtomicU32> {
    let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let g = gain.clone();
    std::thread::spawn(move || {
        let mut reader = platform::Reader::new(); // created here: COM objects are thread-bound
        while !stop.load(Ordering::Relaxed) {
            if let Some(v) = reader.read() {
                g.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    gain
}

// ---- pure parsers (compiled everywhere so they can be tested anywhere) -------

/// `pactl get-sink-volume` + `pactl get-sink-mute` output -> linear gain.
#[allow(dead_code)]
pub fn parse_pactl(volume_out: &str, mute_out: &str) -> Option<f32> {
    if mute_out.to_lowercase().contains("yes") {
        return Some(0.0);
    }
    let pct_pos = volume_out.find('%')?;
    let digits: String = volume_out[..pct_pos]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let pct: f32 = digits.parse().ok()?;
    Some((pct / 100.0).powi(3)) // PulseAudio's volume curve is cubic
}

/// macOS slider position (0-100) -> gain. Apple doesn't document its curve; this is an
/// approximation, so tweak the exponent if the copy feels too loud/quiet.
#[allow(dead_code)]
const MAC_CURVE: i32 = 2;

/// `osascript` output like "50,false" -> linear gain. "missing value,..." -> None.
#[allow(dead_code)]
pub fn parse_osascript(out: &str) -> Option<f32> {
    let (vol, muted) = out.trim().split_once(',')?;
    if muted.trim() == "true" {
        return Some(0.0);
    }
    let v: f32 = vol.trim().parse().ok()?;
    Some((v / 100.0).powi(MAC_CURVE))
}

// ---- platform backends --------------------------------------------------------

#[cfg(windows)]
mod platform {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    pub struct Reader {
        en: Option<IMMDeviceEnumerator>,
    }

    impl Reader {
        pub fn new() -> Self {
            unsafe {
                let ok = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
                let en = if ok { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() } else { None };
                Reader { en }
            }
        }

        pub fn read(&mut self) -> Option<f32> {
            unsafe {
                let dev = self.en.as_ref()?.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
                let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None).ok()?;
                if vol.GetMute().ok()?.as_bool() {
                    return Some(0.0);
                }
                let db = vol.GetMasterVolumeLevel().ok()?;
                Some(10f32.powf(db / 20.0))
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::process::Command;

    pub struct Reader;

    fn run(args: &[&str]) -> Option<String> {
        let out = Command::new("pactl").args(args).env("LC_ALL", "C").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    impl Reader {
        pub fn new() -> Self {
            Reader
        }
        pub fn read(&mut self) -> Option<f32> {
            let vol = run(&["get-sink-volume", "@DEFAULT_SINK@"])?;
            let mute = run(&["get-sink-mute", "@DEFAULT_SINK@"]).unwrap_or_default();
            super::parse_pactl(&vol, &mute)
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::Command;

    pub struct Reader;

    impl Reader {
        pub fn new() -> Self {
            Reader
        }
        pub fn read(&mut self) -> Option<f32> {
            let out = Command::new("osascript")
                .args([
                    "-e",
                    "set s to get volume settings",
                    "-e",
                    "return ((output volume of s) as text) & \",\" & ((output muted of s) as text)",
                ])
                .output()
                .ok()?;
            super::parse_osascript(&String::from_utf8_lossy(&out.stdout))
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod platform {
    pub struct Reader;
    impl Reader {
        pub fn new() -> Self {
            Reader
        }
        pub fn read(&mut self) -> Option<f32> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pactl_percent_is_cubic() {
        let v = "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: 32768 /  50% / -18.06 dB\n        balance 0.00\n";
        let g = parse_pactl(v, "Mute: no\n").unwrap();
        assert!((g - 0.125).abs() < 1e-6, "{g}"); // 0.5^3 == -18.06 dB, matching pactl's own dB
        assert_eq!(parse_pactl(v, "Mute: yes\n"), Some(0.0));
        let full = "Volume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB\n";
        assert!((parse_pactl(full, "Mute: no").unwrap() - 1.0).abs() < 1e-6);
        assert_eq!(parse_pactl("garbage", "Mute: no"), None);
    }

    #[test]
    fn osascript_parsing() {
        assert!((parse_osascript("50,false\n").unwrap() - 0.25).abs() < 1e-6);
        assert_eq!(parse_osascript("50,true"), Some(0.0));
        assert_eq!(parse_osascript("missing value,missing value"), None);
    }
}