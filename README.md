# redmi-buds-linux

Control Redmi / Xiaomi earbuds from Linux without the Xiaomi Earbuds app:
battery, EQ presets, touch controls and Find my buds. Comes as a GTK app and a
command-line tool.

<img src="docs/screenshot.png" width="360" alt="The app showing battery, sound presets, touch controls and Find my buds">

Tested on a **Redmi Buds 6 Play** and a **Redmi Buds 6 Lite**. Other Xiaomi buds
built on the same JieLi-based protocol (Redmi Buds 6 Active, Buds 5, …) will
probably work too, but their feature sets differ. Reports are welcome.

## Features

- Battery for each bud and the case, with charging state
- Noise control: off, noise cancelling and transparency (on models with ANC)
- EQ presets (Balanced, Vocal, Bass, Treble, Volume, Personalized)
- Custom ten-band equalizer with live preview (models that support it, e.g. the 6 Lite)
- Remap single, double and triple tap and press-and-hold, per bud
- Find my buds: ring left, right or both
- Raw access to every device-config id, for exploring

## Install

Requirements: Linux with BlueZ, buds paired with the computer, and Rust ≥ 1.87.
The GUI also needs GTK ≥ 4.12 and libadwaita ≥ 1.7 (with development headers;
on Arch that's `gtk4 libadwaita`, on Debian/Ubuntu
`libgtk-4-dev libadwaita-1-dev`).

```sh
git clone https://github.com/creeperkafasi/redmi-buds-linux
cd redmi-buds-linux
cargo build --release                    # CLI only (zero dependencies)
cargo build --release -p redmi-buds-gui  # GUI

install -Dm755 target/release/redmi-buds     ~/.local/bin/redmi-buds
install -Dm755 target/release/redmi-buds-gui ~/.local/bin/redmi-buds-gui
install -Dm644 gui/data/io.github.creeperkafasi.RedmiBuds.desktop \
    ~/.local/share/applications/io.github.creeperkafasi.RedmiBuds.desktop
```

## Usage

**The buds only serve the control channel to one host at a time.** If your phone
holds them, turn off the phone's Bluetooth (or disconnect the buds there), then
connect from the computer: `bluetoothctl connect <MAC>`.

The GUI finds paired buds by itself, keeps one session open, refreshes the
battery every 20 s and reconnects on its own. Pass `--mac <ADDR>` to pick a
device.

```sh
redmi-buds battery                     # L=100%⚡  R=100%⚡  case=40%
redmi-buds info
redmi-buds eq get
redmi-buds eq set 5                    # 0 balanced, 1 vocal, 5 bass, 6 treble,
                                       # 7 volume, 10 personalized
redmi-buds eq custom get               # the custom curve (config id 55)
redmi-buds eq custom set "-3,0,0,0,0,2,4,4,2,1"   # one gain per band
redmi-buds eq custom set "62=-6,16000=3"          # or just some bands
redmi-buds eq custom reset             # flat curve
redmi-buds anc get                     # anc mode 1 (Noise cancelling)  level 0
redmi-buds anc set transparency        # off | noise | transparency
redmi-buds anc set noise 2             # optional level 0-2, model specific
redmi-buds gesture get
redmi-buds gesture set double left next
redmi-buds find                        # ring; `find --stop` to stop
redmi-buds get 7 47                    # raw config ids
redmi-buds set 47 01                   # e.g. low-latency (game) mode on
redmi-buds selftest                    # auth check, no device needed
```

Options: `--mac <ADDR>` (default: first paired device with "Buds" in its name),
`--channel <N>`, and `-v` to log every frame. While the GUI is open it holds the
connection, so the CLI can't connect until you close it.

## How it works

The buds speak a Xiaomi variant of JieLi's RCSP protocol over Bluetooth Classic
RFCOMM. Sessions start with a challenge/response handshake whose transform was
recovered from the app's native library and reimplemented here. It needs no
per-device secrets. **[PROTOCOL.md](PROTOCOL.md)** documents the framing, the
auth transform, the opcodes, every device-config id, and what the firmware
actually supports beyond what the app shows.

| path | contents |
|---|---|
| `src/` | library + CLI, zero dependencies (raw `AF_BLUETOOTH` FFI) |
| `src/auth.rs` | the auth transform |
| `src/settings.rs` | EQ preset names, gesture table encoding |
| `gui/` | GTK 4 / libadwaita app |
| `examples/probe.rs` | protocol exploration tool (scan ids, raw commands, write-and-restore) |
| `python/` | the Python prototype the Rust port was validated against |

`cargo test` covers framing, gesture/battery decoding, the Bluetooth address
layout, and the auth transform: hardware-recorded responses plus 1000 vectors
from the Python version.

## Disclaimer

An independent interoperability project. Not affiliated with or endorsed by
Xiaomi. Use at your own risk; this tool avoids firmware-update and reset
commands, but a misbehaving client can still confuse the buds (re-pairing fixes
that).

## License

MIT
