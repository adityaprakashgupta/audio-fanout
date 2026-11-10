# audio-fanout

Play one audio stream on **any number of output devices at once**, in sync.

Send your system audio to a virtual device, and `audio-fanout` copies it to your
speakers, wired headphones, Bluetooth earbuds, HDMI display and anything else you
choose. It keeps the devices locked together over long sessions, lets you delay
the fast ones to match slow Bluetooth, and follows your system volume.

Written in Rust. Cross-platform through [cpal](https://github.com/RustAudio/cpal).
No kernel driver and no administrator rights needed.

```
 Apps ──▶ [ virtual device ] ──▶ audio-fanout ─┬─▶ Speakers
                                                ├─▶ Headphones (wired)
                                                ├─▶ Bluetooth earbuds   (+ delay)
                                                └─▶ HDMI / anything else
```

## Why

Some laptop audio suites can mirror sound to two outputs. This does it for as many
as you like, on any OS, with per-device delay and volume.

## Features

- **Any number of outputs**, each on its own audio thread. One failing device
  does not stop the others.
- **Drift correction.** Every device has its own hardware clock, so naive copying
  slowly falls out of sync. Each output watches its buffer fill level and nudges
  a high-quality resampler ([rubato](https://crates.io/crates/rubato)) by a few
  parts per million, with no clicks.
- **Per-output delay** to align Bluetooth (often 150 ms or more) with wired
  devices.
- **Per-output volume**, plus **system volume following** (volume keys and mute
  work on the copy).
- **Rate and channel conversion** between source and each device
  (for example 48 kHz stereo into 44.1 kHz mono).
- **Real-time safe** audio callbacks: no allocation, no locks.

## How it works

```
source stream ─▶ one ring buffer per output
                      │
                      ▼   (per output, in the device's audio callback)
        prime to delay ─▶ drift controller ─▶ rubato resampler
                      ─▶ channel remap ─▶ gain (output x system volume) ─▶ device
```

The source device's clock is the master and every output is a slave. For each
output, a small controller compares how full its ring buffer is against a target
(`delay + 20 ms`) and adjusts the resampling ratio. A device that runs 100 ppm
slow ends up with a ratio around 0.9999, which is inaudible but keeps it aligned
indefinitely. After silence or an underrun, the output re-primes so the delay is
rebuilt before audio resumes.

## Setup

The source is always an **input device**, because that is the one thing every OS
and cpal can capture uniformly. You make one by installing a virtual audio device,
then setting it as your system's default **output**.

| OS | Install | Source (`--source`) | Set as default output |
|---|---|---|---|
| Windows | [VB-Cable](https://vb-audio.com/Cable/) | `CABLE Output` | `CABLE Input` |
| macOS | [BlackHole](https://github.com/ExistentialAudio/BlackHole) | `BlackHole 2ch` | `BlackHole 2ch` |
| Linux | a PulseAudio/PipeWire null sink (below) | its monitor source | the null sink |

On Windows you can also pass a plain output device name as `--source` to capture
it by WASAPI loopback, which is handy for a quick test without installing
anything.

Do not use a Multi-Output device as the default output on macOS: macOS cannot
change the volume of a Multi-Output device.

### Linux null sink (experimental)

```sh
pactl load-module module-null-sink sink_name=fanout sink.properties=device.description=Fanout
# set "Fanout" as the default output in your sound settings, then:
PULSE_SOURCE=fanout.monitor cargo run --release -- --source pulse --out ...
```

How cpal exposes monitor sources differs between systems. Run `--list` to see what
you get, and adjust.

## Install and build

```sh
cargo build --release
```

- **Linux:** needs the ALSA headers (`sudo apt install libasound2-dev`) and
  `pactl` at runtime for volume following (`pulseaudio-utils`, or PipeWire's
  PulseAudio layer).
- **macOS:** nothing extra. Volume following uses the built-in `osascript`.
- **Windows:** nothing extra.

## Usage

```sh
audio-fanout --list
audio-fanout --source "<source>" --out "<name>[:delay_ms[:volume_percent]]" [--out ...]
```

Names are case-insensitive substrings of what `--list` prints.

| Option | Meaning |
|---|---|
| `--list` | Print input devices (sources) and output devices |
| `--source NAME` | The device audio is copied from |
| `--out NAME[:DELAY[:VOL]]` | An output. `DELAY` in milliseconds, `VOL` in percent. Repeat for each output |
| `--no-follow-volume` | Ignore the system volume and play at full level |
| `FANOUT_DEBUG=1` (env) | Every 5 s, print each output's buffer fill and resample ratio |

Example (Windows):

```sh
audio-fanout --source "CABLE Output" \
  --out "Realtek:180" \
  --out "Headphones:0:50" \
  --out "Earbuds:20:50"
```

Press Enter to quit.

### Aligning the delays

A delay can only add latency, so the slowest device gets `:0` and every other
device is delayed to match it.

1. Play something with sharp clicks (a metronome video works well) with all
   outputs at `:0`.
2. Compare two devices at a time. Raise the delay of whichever is earlier in
   20 ms steps until the echo disappears.
3. Bluetooth usually trails wired devices by 150-250 ms, so start there.
4. Write the values down. They stay valid for that device and codec.

Windows cannot see the radio and codec delay of Bluetooth devices, so there is no
reliable way to measure it automatically. Tune it by ear.

## Volume

"System volume" means the volume of your OS **default output device**, which is
what your volume keys and tray slider control. A background thread reads it four
times a second and scales every output by it, on top of each output's own
`:volume`.

| OS | How it is read |
|---|---|
| Windows | Endpoint volume API (exact dB attenuation) |
| Linux | `pactl` (percent, cubic curve as PulseAudio uses) |
| macOS | `osascript` (approximate curve) |

Caveats:

- On macOS, virtual devices may not expose a volume control. If
  `osascript -e 'output volume of (get volume settings)'` prints `missing value`
  while your virtual device is the default, following has nothing to read and the
  copy stays at full level. Use the per-output `:volume` instead.
- If the result is much quieter than expected, your virtual device may already
  apply the volume itself. Run with `--no-follow-volume` and compare.
- Each device's own OS volume still applies on top, so keep wired speakers at
  100% in the OS and control loudness with the system volume.

## Limitations

- Requires `f32` sample format. Common backends offer it. A raw ALSA hardware
  device that only offers integers is rejected with a message.
- No automatic reconnect: if a Bluetooth device drops, that output stops (the
  others continue) until you restart.
- Downmix and upmix are simple (average to mono, duplicate to stereo, extra
  channels silent), not full surround handling.
- Never choose the virtual device's own playback side as an output, or you create
  a feedback loop. Only loopback sources are checked automatically.
- You may not be allowed to redistribute VB-Cable or BlackHole with your own
  software. Check their licenses before you ship an installer.

## Status

The drift controller, delay, rate and channel conversion, and volume ramping are
covered by unit tests (`cargo test`) that run without any audio hardware, and the
volume parsers are tested for Linux and macOS output formats. The code type-checks
for Windows (MSVC).

Windows is the primary development platform. macOS and Linux paths are written
but have not yet been confirmed on real hardware. Reports and fixes are welcome.

## Testing

```sh
cargo test --release
```

## Roadmap

- Reconnect outputs automatically when devices appear and disappear
- Save outputs, delays and volumes in a config file
- Tray or small GUI for volume and delay
- Terminal master-volume keys for platforms without a system volume to follow
- Move to cpal 0.18 (the API changed from the 0.15 used here)
- A custom macOS virtual device with volume control, if existing ones fall short

## License

Choose a license for the project (for example MIT or Apache-2.0) and add a
`LICENSE` file.
