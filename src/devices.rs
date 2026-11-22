//! Device lookup helpers shared by main and the connectors.

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, SampleFormat, StreamConfig, SupportedStreamConfigRange};

pub fn name_of(d: &Device) -> String {
    d.name().unwrap_or_else(|_| "<unknown>".into())
}

/// Does device `name` match `needle`? (exact, or case-insensitive substring)
pub fn name_matches(needle: &str, name: &str) -> bool {
    name == needle || name.to_lowercase().contains(&needle.to_lowercase())
}

/// The device whose name equals `needle`, else the first whose name contains it.
/// Exact-first matters because the UI adds devices by their full name.
pub fn find(devs: impl Iterator<Item = Device>, needle: &str) -> Option<Device> {
    let mut v: Vec<Device> = devs.collect();
    let n = needle.to_lowercase();
    let idx = v
        .iter()
        .position(|d| name_of(d) == needle)
        .or_else(|| v.iter().position(|d| name_of(d).to_lowercase().contains(&n)))?;
    Some(v.swap_remove(idx))
}

/// Full name of the output device matching `needle`, if it is present right now.
pub fn resolve_output_name(host: &Host, needle: &str) -> Option<String> {
    find(host.output_devices().ok()?, needle).map(|d| name_of(&d))
}

fn norm(s: &str) -> String {
    let base = s.split('(').next().unwrap_or(s).to_lowercase();
    base.split_whitespace().filter(|w| *w != "input" && *w != "output").collect::<Vec<_>>().join(" ")
}

/// Would playing on `out` feed back into `source`? True for the two halves of a
/// virtual cable ("CABLE Output" / "CABLE Input"), a device that is both, and a
/// loopback source's own output.
pub fn loops_back_to(source: &str, out: &str) -> bool {
    let a = norm(source);
    !a.is_empty() && a == norm(out)
}

/// Pick an f32 config at the device's default rate/channels.
/// `output_side`: read the *output* side of the device (WASAPI loopback sources).
pub fn f32_config(dev: &Device, output_side: bool) -> Result<StreamConfig> {
    let def = if output_side { dev.default_output_config()? } else { dev.default_input_config()? };
    if def.sample_format() == SampleFormat::F32 {
        return Ok(def.config());
    }
    let ranges: Vec<SupportedStreamConfigRange> = if output_side {
        dev.supported_output_configs()?.collect()
    } else {
        dev.supported_input_configs()?.collect()
    };
    let r = ranges
        .into_iter()
        .find(|r| {
            r.sample_format() == SampleFormat::F32
                && r.channels() == def.channels()
                && r.min_sample_rate() <= def.sample_rate()
                && def.sample_rate() <= r.max_sample_rate()
        })
        .ok_or_else(|| anyhow!("\"{}\" offers no f32 format", name_of(dev)))?;
    Ok(r.with_sample_rate(def.sample_rate()).config())
}

/// Find the source: an input device, or (Windows only) an output device captured
/// by loopback. Returns the device and whether it is a loopback source.
pub fn resolve_source(host: &Host, needle: &str) -> Result<(Device, bool)> {
    if let Some(d) = find(host.input_devices()?, needle) {
        return Ok((d, false));
    }
    if cfg!(windows) {
        if let Some(d) = find(host.output_devices()?, needle) {
            return Ok((d, true));
        }
        bail!("no input or output device matching \"{needle}\" (try --list)");
    }
    bail!("no input device matching \"{needle}\" (try --list)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_feedback_pairs() {
        assert!(loops_back_to("CABLE Output (VB-Audio Virtual Cable)", "CABLE Input (VB-Audio Virtual Cable)"));
        assert!(loops_back_to("BlackHole 2ch", "BlackHole 2ch"));
        assert!(loops_back_to("Speakers (Realtek(R) Audio)", "Speakers (Realtek(R) Audio)"));
        assert!(!loops_back_to("CABLE Output (VB-Audio Virtual Cable)", "Speakers (Realtek(R) Audio)"));
        assert!(!loops_back_to("CABLE Output (VB-Audio Virtual Cable)", "Headphones (OnePlus Bullets Wireless Z2)"));
        assert!(!loops_back_to("", ""));
    }

    #[test]
    fn matching_rules() {
        assert!(name_matches("realtek", "Speakers (Realtek(R) Audio)"));
        assert!(name_matches("Speakers (Realtek(R) Audio)", "Speakers (Realtek(R) Audio)"));
        assert!(!name_matches("onePlus", "Speakers (Realtek(R) Audio)"));
    }
}
