//! Device lookup helpers shared by main and the connectors.

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host, SampleFormat, StreamConfig, SupportedStreamConfigRange};

pub fn name_of(d: &Device) -> String {
    d.name().unwrap_or_else(|_| "<unknown>".into())
}

/// First device whose name contains `needle` (case-insensitive).
pub fn find(devs: impl Iterator<Item = Device>, needle: &str) -> Option<Device> {
    let n = needle.to_lowercase();
    devs.into_iter().find(|d| name_of(d).to_lowercase().contains(&n))
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