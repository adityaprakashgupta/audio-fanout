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

- **Interactive terminal UI** (ratatui): add and remove outputs from a live
  dropdown while audio is playing, and change each output's **volume and latency
  on demand**. The device list updates as hardware is plugged in or removed.
- **Any number of outputs**, each on its own audio thread. One failing device
  does not stop the others.
- **Drift correction.** Every device has its own hardware clock, so naive copying
  slowly falls out of sync. Each output watches its buffer fill level and nudges
  a high-quality resampler ([rubato](https://crates.io/crates/rubato)) by a few
  parts per million, with no clicks.
- **Automatic reconnection.** An output that is switched off, unplugged or out of
  range is retried in the background and resynced when it returns. The other
  outputs never notice. Outputs that are missing at startup are simply waited for.
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

## Connection handling

Every output, and the source, runs under its own small supervisor thread that
connects, watches the stream and reconnects it.

```
find device ─▶ configure ─▶ start stream ─▶ watch ──(error / device gone)──┐
     ▲                                                                      │
     └──────────── retry with backoff (0.5 s, 1 s, 2 s ... max 5 s) ◀───────┘
```

- **Detection.** A stream error from the OS marks the connection dead. As a
  backstop, every 2 seconds the supervisor also checks that the device still
  appears in the OS device list.
- **Resync, not restart.** Each output owns its buffer, and the source is the
  shared timeline. When a device returns, only that output is rebuilt: its stream
  is reopened, stale audio is flushed, the drift controller is reset and the
  output waits until its delay is buffered again from *live* audio. Delays
  therefore line up with the other outputs without touching them.
- **New format on return.** The device may come back with a different sample rate
  or channel count (Bluetooth often does this when the codec changes). The
  resampler and channel mapping are rebuilt for the new format automatically.
- **Source loss.** If the source drops, the outputs play silence and re-prime
  together when it returns. The source must exist at startup, and it must come
  back in the same format, otherwise you get a clear message asking for a
  restart (the buffers are sized for the original format).
- **Quiet logs.** State changes are printed once (`connected`, `lost`,
  `reconnected`), not on every retry.

If a Bluetooth device renegotiates its codec on reconnect, its latency can change.
The resync is correct, but you may need to re-tune that device's `:delay`.

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

### Terminal UI (default)

```sh
audio-fanout                             # pick the source on screen
audio-fanout --source "CABLE Output"     # skip the source screen
audio-fanout --source "CABLE Output" --out "Speakers:180" --out "Earbuds:0:50"   # preload outputs
```

```
┌ audio-fanout ──────────────────────────────────────────────────────────────┐
│ Source: CABLE Output (VB-Audio Virtual Cable) (48000 Hz, 2 ch) ●           │
│     system volume: following (x72%)                                        │
├ Outputs ───────────────────────────────────────────────────────────────────┤
│ Status    Output                       Volume          Delay     Buffer    │
│ ● live    Speakers (Realtek(R) Audio)  ████████░░  80% +180 ms  38/200 ms  │
│ ● live    Headphones (OnePlus Bullets) █████░░░░░  50%   +0 ms   35/40 ms   │
│ ◌ waiting Headphones (Airdopes Alpha)  █████░░░░░  50%  +20 ms   -          │
├ Log ───────────────────────────────────────────────────────────────────────┤
│ [00:00:03] [Speakers (Realtek(R) Audio)] connected: ... (48000 Hz, 2 ch)   │
└────────────────────────────────────────────────────────────────────────────┘
```

| Key | Action |
|---|---|
| `↑` `↓` (or `k` `j`) | Select an output |
| `←` `→` (or `h` `l`) | Volume down / up by 5% |
| `-` `+` | Volume down / up by 1% |
| `[` `]` | Delay down / up by 10 ms |
| `{` `}` | Delay down / up by 50 ms |
| `m` | Mute / unmute the selected output |
| `a` | Add an output: opens a dropdown of the devices present right now |
| `d` or `Del` | Remove the selected output |
| `f` | Follow / stop following the system volume |
| `q` or `Ctrl-C` | Quit |

**Adding and removing outputs live.** Press `a` for a dropdown of output devices.
The list refreshes every second, so a device you plug in or switch on appears
while the dropdown is open, and one that disappears vanishes from it. The
highlight stays on the device you chose even when the list changes around it.
Devices already in the group are hidden, and so is any device that would feed
back into the source (such as `CABLE Input` when the source is `CABLE Output`).
Nothing restarts when you add or remove an output; the others keep playing.

**Changing volume and delay live.** Changes apply immediately while you listen.
Volume and mute ramp smoothly. Increasing a delay causes a short silence on that
output equal to the change (it has to buffer more audio), and decreasing it skips
the surplus audio with a quick fade-in. The delay maximum is 1500 ms.
Outputs added from the UI start at 50% volume, so new earbuds don't start at full
blast.

If you give no `--source`, the first screen lists input devices (and, on Windows,
output devices that can be captured by loopback) so you can pick one.

### Headless / scripting

```sh
audio-fanout --no-tui --source "<source>" --out "<name>[:delay_ms[:volume_percent]]" [--out ...]
```

Logs go to stdout and Enter quits. This uses the same engine as the TUI.

| Option | Meaning |
|---|---|
| `--list` | Print input devices (sources) and output devices |
| `--source NAME` | The device audio is copied from |
| `--out NAME[:DELAY[:VOL]]` | An output. `DELAY` in milliseconds, `VOL` in percent. Repeat for each output |
| `--no-follow-volume` | Start without following the system volume |
| `--no-tui` | Run without the terminal UI |

Names are case-insensitive substrings of what `--list` prints.

Example (Windows):

```sh
audio-fanout --no-tui --source "CABLE Output" \
  --out "Realtek:180" \
  --out "Headphones:0:50" \
  --out "Earbuds:20:50"
```

### Aligning the delays

A delay can only add latency, so the slowest device gets `:0` and every other
device is delayed to match it. In the TUI you can adjust the delays with `[` `]`
while listening, which is the easiest way to line them up.

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
- The source must be present at startup, and must return with the same format
  after a drop. Outputs have no such limits.
- Disconnects are noticed by stream errors plus a 2-second device-list check, not
  by OS hot-plug notifications, so detection can take up to a couple of seconds.
- Downmix and upmix are simple (average to mono, duplicate to stereo, extra
  channels silent), not full surround handling.
- Never choose the virtual device's own playback side as an output, or you create
  a feedback loop. The TUI hides the obvious cases (the two halves of a virtual
  cable, or a device that is both input and output), but the check goes by name,
  so it cannot catch every setup.
- The source cannot be changed while running; restart to use another one.
- Settings (outputs, delays, volumes) are not saved between runs yet.
- On Linux, ALSA sometimes prints diagnostics straight to the terminal, which can
  garble the TUI. Run it as `audio-fanout 2>/dev/null` if you see that.
- You may not be allowed to redistribute VB-Cable or BlackHole with your own
  software. Check their licenses before you ship an installer.

## Status

The drift controller, delay, rate and channel conversion, volume ramping and the
reconnect state machine (device absent at startup, unplugged, plugged back in,
prompt shutdown) are covered by unit tests (`cargo test`) that run without any
audio hardware, and the volume parsers are tested for Linux and macOS output
formats. The TUI's key handling and drawing are tested without a terminal (a fake engine
and ratatui's test backend), including the live dropdown following a device by
name as the list changes. Live delay and volume changes are covered by audio
tests. Reconnection and the TUI have not yet been exercised against real devices. The code type-checks
for Windows (MSVC).

Windows is the primary development platform. macOS and Linux paths are written
but have not yet been confirmed on real hardware. Reports and fixes are welcome.

## Testing

```sh
cargo test --release
```

## Roadmap

- Use OS hot-plug notifications for instant detection instead of polling
- Let the source change format on reconnect
- Save outputs, delays and volumes in a config file
- Remember outputs, delays and volumes between runs
- Terminal master-volume keys for platforms without a system volume to follow
- Move to cpal 0.18 (the API changed from the 0.15 used here)
- A custom macOS virtual device with volume control, if existing ones fall short

## License

Choose a license for the project (for example MIT or Apache-2.0) and add a
`LICENSE` file.