# Redmi Buds 6 Play — control protocol notes

Source: static analysis of `com.mi.earphone` 1.38.0i (`com.xiaomi.aivsbluetoothsdk`,
a JieLi RCSP derivative) **plus a verified HCI capture of the phone↔buds session
and a live desktop client**.

Everything marked **verified** below was observed on the wire and/or exercised
against real hardware. The ground truth is an Android HCI snoop log of the
official app talking to the buds (not published here: it contains the device's
addresses).

Clients: the Rust crate in this repository, and `python/redmi_buds.py`.
Auth transform: `src/auth.rs` and `python/xiaomi_auth.py`.

---

## Transport

Bluetooth Classic RFCOMM. The app probes these SDP UUIDs in order:

| Name | UUID | Channel on this device |
|---|---|---|
| MIUI (primary) | `0000fd2d-0000-1000-8000-00805f9b34fb` | **6** |
| Serial Port (SPP) | `00001101-0000-1000-8000-00805f9b34fb` | 1, 2, 5 |
| XiaoAI | `00001101-0000-1000-8000-008584d01810` | — |

**Verified:** on the Redmi Buds 6 Play the RCSP control channel is RFCOMM channel
**6**, the one advertised under the `fd2d` UUID. On the Redmi Buds 6 Lite the same
service is on channel **29** (`sdptool` is not needed to find it: the client just
tries both). The plain-SPP channels 1/2/5 are also open on the 6 Play but are not
the RCSP endpoint the app uses (`sdptool browse` lists all of them).

Beware: not every channel that accepts an RFCOMM connection speaks RCSP. On the
6 Lite, channel 12 connects and then stays silent — probing it by hand looks like
a failed auth handshake rather than a closed channel.

The device also exposes BLE (`0000af00-…`) and `df21fe2c-…`, used for the BLE-only
buds. Audio runs independently over A2DP.

## Frame

```
FE DC BA | flags | opcode | len (u16 BE) | payload[len] | EF
```

- No checksum, no byte-stuffing. Scan for `FE DC BA`, read `len`, expect `EF`.
- `flags`:
  - bit 7: 1 = command (phone → buds), 0 = response
  - bit 6: 1 = sender expects a response
  - bits 0–2: target app id

Payload layout by direction:

```
command  : sn | [xm_opcode if opcode == 0x01] | params…
response : status | sn | data…
```

`status`: 0 ok, 1 fail, 2 unknown cmd, 3 busy, 4 no resource, 5 crc, 7 param error,
9 not supported.

### Target app ids

From `BasePacket.ID_PKG_MAP`:

| id | owner |
|---|---|
| 0 | XiaoAI / case / earphone |
| 1 | `com.xiaomi.bluetooth` (MIUI BT stack) |
| 2 | `com.xiaomi.wearable` |
| 3 | `com.milink.service` |
| 4 | `com.mi.earphone` (the earphone app) |
| 5 | `com.xiaomi.finddevice` |

**Verified usage:** the auth exchange runs under app id **1**, all device-config
traffic under app id **4**. The buds' own responses echo an app id of their
choosing (often 1 or 7); do not pattern-match on it for responses.

## Session start / authentication — CORRECTED

> Earlier notes described a raw, *unframed* six-message handshake
> (`00‖rand`, `01‖resp`, `02"pass"`). **That does not happen on this device.**
> A full capture from connection setup to teardown contains **zero** unframed
> auth payloads; every byte on the control channel is an RCSP frame. The
> `00/01/02` handling in `BluetoothAuth.java` is a dead/legacy SPP path.

Authentication is carried by two framed opcodes, `AUTH_CHECK` (0x50) and
`AUTH_SEND_CALC_RESULT` (0x51):

```
phone -> buds : 0x50 cmd, app 1, params = 01 ‖ challenge[16]
buds  -> phone: 0x50 rsp, status 0, data = 01 ‖ transform(challenge)[16]
phone -> buds : 0x51 cmd, app 1, params = 01 00     # version 1, pairResult 0
buds  -> phone: 0x51 rsp, status 0
… then `0xF2/0xF3` config commands are accepted
```

`01` is a protocol-version byte. The phone compares the buds' response with its
own computation of `transform(challenge)`; if they differ, auth fails.

**Bidirectional case (verified live):** the buds may *also* send its own
`0x50 cmd` with its own 16-byte challenge. Reply with a `0x50 rsp` whose payload
is `00 | sn | 01 ‖ transform(challenge)`. This was observed and answered
successfully by `redmi_buds.py`.

### The transform

Implemented in **`python/xiaomi_auth.py`** and **`src/auth.rs`**. Neither needs a
vendor binary, an emulator or a cross-compiled helper. It was recovered from a
disassembly of the app's `libxm_bluetooth.so` (aarch64) and validated against both
the real library and recorded hardware traffic.

The library exports three entry points:

```
function_E1test(addr6, challenge16, key16, out16)   # result is discarded
function_E21   (addr6, challenge16, out16)          # the actual transform
function_xiaomi(addr6, challenge16, key16, out16)   # calls E1test, then E21
```

`function_E21` overwrites `out` with an address-derived pattern *before* mixing,
so E1test's output never reaches the result: **the whole transform is exactly
E21**. (Verified: E21 returns the same 16 bytes for any incoming buffer contents
and equals the full `function_xiaomi` output.)

```
transform(addr6, challenge16):
    out   = addr6 ‖ addr6 ‖ addr6[:4]        # 16 bytes
    tmp   = challenge16 ;  tmp[15] ^= 0x06
    sched = expand_key(tmp)                  # 272 bytes
    return mix(out, sched, flag=1)
```

The two fixed constants the earphone app always passes:

| argument | value | notes |
|---|---|---|
| `addr6` | `11 22 33 33 22 11` | **placeholder** from `.data 0xa6c0` — not the MAC |
| `key16` | `06 77 5f 87 91 8d d4 23 00 5d f1 d8 cf 0c 14 2b` | factory key at `.data 0xa6b0` |

Two things worth noting, both settled empirically:

- **The real Bluetooth MAC is not used.** `function_xiaomi` *does* mix its `addr6`
  argument in (changing it changes the output), but the app always passes the
  fixed placeholder, so every device gets the same treatment.
- **`setLinkKey` is never called.** It writes 16 bytes over the key at
  `.data 0xa6b0`, i.e. it replaces the *key*, not the address placeholder. Its
  only Java entry point (`BluetoothAuth.n()`) has no callers anywhere in the
  decompiled tree, so the factory key stands.

#### expand_key (sub_1630)

Zeros 272 bytes, stores the key at `buf[0:16]`, then runs 16 rounds. Each round
rotates the 16-byte state and an 8-bit checksum left by 3, forming a 17-byte
rolling window (state + checksum byte); 16 output bytes are produced from
`table[block + 15 - j] + window[(round + 1 + j) % 17]`, where `table` is the
256 bytes at `.rodata 0x970`.

The checksum seed is `XOR` of `key[i] ^ key[i+8]` for `i` in 0..7.

#### mix (sub_176c)

Eight rounds. Each round:

1. (only round 2, only when `flag` is set) combine with the saved input state
2. combine with `sched[32*r + i]`
3. substitute each byte through one of two 256-byte tables at `.rodata 0xa70`
   and `0xb70`
4. combine with `sched[32*r + 16 + i]`
5. apply an unrolled 16-byte linear mixing block (`b[i] + 2*b[j]` butterflies, mod 256)

then a final combine with `sched[256 + i]`.

Steps 2 and 4 select `xor` vs `add` per byte from the mask `0x9999` — note step 4
uses the **opposite** sense to step 2. The same mask picks which of the two
substitution tables a byte goes through (set → `0xa70`, clear → `0xb70`).

#### Verification

- `python3 python/xiaomi_auth.py` and `redmi-buds selftest` check three
  challenge/response pairs recorded from the real buds.
- During development every primitive (`expand_key`, `mix`, the full transform)
  was differential-tested against the vendor library itself, called in-process
  through a small harness, over ~1200 random inputs. That harness relies on the
  proprietary library and is not published.
- `cargo test` checks `src/auth.rs` against 1000 vectors from the Python version.

## Opcodes

| Hex | Name |
|---|---|
| 01 | DATA (with sub-opcode) |
| 02 | GET_TARGET_INFO |
| 03 | REBOOT_DEVICE |
| 04 | NOTIFY_DEVICE_APP_INFO |
| 06 / 0B | disconnect / wake classic BT |
| 08 | SET_TARGET_INFO |
| 09 | GET_DEVICE_RUN_INFO |
| 0E | REPORT_DEVICE_STATUS (buds → phone) |
| 42 | FIND_DEVICE (not used by the earphone app, see config id 9) |
| 50 / 51 | AUTH_CHECK / AUTH_SEND_CALC_RESULT |
| C1–C4 | log upload |
| D0–D3 | speech start/stop/cancel/long-hold |
| E1–E7 | OTA |
| F0 / F1 | vendor (JieLi S18 / extend) |
| **F2** | **SET_DEVICE_CONFIG** |
| **F3** | **GET_DEVICE_CONFIG** |
| **F4** | **NOTIFY_DEVICE_CONFIG** (buds → phone) |
| FA / FB | blob message / chunk |

## Commands verified on the wire

### GET_TARGET_INFO (0x02)

Request params: `FF FF FF FF` (all attributes). Response data is a list of
attributes in `len(u8) | type(u8) | value(len-1)` form — note the **one-byte**
type here, unlike device config below.

| type | meaning | observed value |
|---|---|---|
| 0 | name | `Redmi Buds 6 Play` |
| 1 | version | `20 22 20 22` |
| 2 | quantity (battery %) | `0a` |
| 3 | VID / PID | `2717 509b` (Xiaomi / Buds 6 Play) |
| 5 | mandatory upgrade flag | `00` |
| 6 | uboot version | `02 02` |
| 7 | multi-battery | `e4 e4 32` |
| 13 | color type | `02` |

The app's complete type list (from the parse switch): 0 name, 1 version,
2 quantity, 3 vid/pid, 4 —, 5 mandatory upgrade, 6 uboot version, 7 multi-battery,
8 codec type, 9 power mode, 10 function key, 11 hot word, 12 charset, 13 color,
14 spatial-audio type, 15 virtual-surround type, 16 bulk-data support.

### GET_DEVICE_RUN_INFO (0x09)

Request params `FF FF FF FF`; same `len(u8) | type(u8) | value` attribute list.
Observed: type 0 = EDR address, 1 = BLE address, plus small fields 2–6 and 0x0b
(ANC status, EDR status, power mode, TWS role, auto-play).

Raw example:
`07 00 <edr addr 6>  07 01 <ble addr 6>  02 02 fb  02 03 01  02 04 00  02 05 00  02 06 00  02 0b 00`

### Battery

Type 7 (multi-battery) is a plain byte array; `DeviceElectricInfo` decodes each
entry as:

- `percent = v & 0x7F`
- `charging = (v & 0x80) != 0`
- `0xFF` = unknown (skipped)
- order is `[left, right, case]`

So `e4 e4 32` = left 100 % (charging), right 100 % (charging), case 50 %.
**Verified live** against the buds.

The third byte is only meaningful while the buds sit in the case. On the Buds 6
Lite the same read gives `e1 e3 27` (97 %/99 % charging, case 39 %) docked but
`61 63 ff` undocked: the firmware reports `0xFF` for the case as soon as the buds
are out, and the earphone app then hides the case column entirely. So "case
unknown" is the normal state when the buds are being worn, not a read failure.

Type 2 (`quantity`) is a separate single value and is not what the app displays.

## Device config (0xF2 / 0xF3 / 0xF4)

- **GET (0xF3)** params: list of `u16 BE` config ids.
- **SET (0xF2) / NOTIFY (0xF4)** params: concatenated TLVs.

Config TLV — note the **two-byte** type here:

```
len (u8, = 2 + value_len) | type (u16 BE) | value…
```

Special case type 52: `FF | type u16 | len u16 | value…`.

### Config ids verified on this device

The buds answered reads for ids **1, 2, 3, 4, 7, 10, 11, 12, 13, 15, 30, 35,
37, 39, 40, 55, 83, 93**. Ids the app asks for at startup:

```
00 01  00 02  00 03  00 04  00 0a  00 0b  00 0f  00 0d  00 25  00 1e  00 23  00 27
```

Observed values and their encodings:

| Id | Setting | Verified value / encoding |
|---|---|---|
| 2 | gesture click set | 12 bytes = repeated triples `[setType, left, right]`; setType 1 double, 2 triple, 3 long-press, 4 single, 5 slide. Observed `04 0804 / 01 0101 / 02 0303 / 03 0808`. **Write** one triple, `FF` = untouched side (verified live). Actions: 0 voice assistant, 1 play/pause, 2 previous, 3 next, 4 vol+, 5 vol−, 6 noise control, 8 none, 9 photo, 10 record, 11 volume, 12 translate, 13 prev/next |
| 3 | auto-answer | `00` |
| 7 | **sound effect / EQ preset** | one byte. App names: 0 balanced, 1 vocal enhancement, 5 bass boost, 6 treble boost, 7 volume boost (`SoundEffectExtKt`). 2–4 are accepted but unnamed |
| 9 | **find device (ring)** | `[state, deviceType]`; state 1 = ring, 0 = stop; deviceType 1 left, 2 right, 3 both. Used by the app's Find Device screen instead of opcode 0x42 |
| 10 | long-press noise cycle | `00 00` on the 6 Play; `07 07` on the 6 Lite: one bitmask byte per side, bit 0 offline, bit 1 ANC, bit 2 transparency |
| 11 | ANC state + level | `00 00` on the 6 Play (ignored); on the 6 Lite `[mode, level]`, see below |
| 37 | adaptive ANC | `00` |
| 39 | serial number | ASCII, `<5 digits>/<14 alphanumerics>` |
| 40 | time | 4 bytes, Unix-ish (`6a e5 ea 70`) |
| 55 | **custom EQ** | report + set/preview/reset TLVs, see below. `00` on the 6 Play (feature absent) |
| 83 | click set (newer models) | `DeviceConfigClickSet` in `setting/function/` → id 83. **Not** the class used for these buds |
| 93 | restricted | `00` |

`props` of interest from the full id table remain in the app for other models:
5 fit test, 29/36/79 spatial audio / virtual surround / personal spatial,
41 adaptive sense, 47 low latency (game mode), 54 scene rendering,
58 notify sound, 59 personalized noise reduction, 60/62 ear-canal fit,
72 adaptive volume, 74 bluetooth info, 76 app country, 103 commuting immerse,
114/115 case sounds, 120 translate, 161 notification volume.

### Noise control (verified on the Buds 6 Lite, 2026-11-01)

The mode is **not** a device-config write. `SET_TARGET_INFO` (0x08) attribute 4
carries it as a vendor TLV `len(u8) | type(u8) | value`:

```
02 04 00    off
02 04 01    noise cancelling
02 04 02    transparency
```

The firmware answers `status = 0` and mirrors the new mode into config id 11 by
itself. Config id 11 therefore holds `[mode, level]` and is only the stored
state:

| value | mode (earphone app 1.38.0i) |
|---|---|
| 0 | Off |
| 1 | Noise cancelling |
| 2 | Transparency |
| 3 | stored by the firmware, no such mode in the app |

**Writing config id 11 alone does nothing audible.** The value is stored, the
earphone app then reports the new mode, and the read-back agrees — but the DSP
never switches. This is a good trap: every observable signal says the write
worked. Only the attribute above moves the audio path, which is why the desktop
client has to send it (`settings::encode_anc_switch`).

The level byte is a second axis: the firmware stores 0–2 for modes 1 and 2 and
resets it to 0 on a mode change. It has no attribute; the app writes it through
config id 11 (`DeviceConfigNoiseLevel`, `paramsToValue` = `[state, level]`), so
that write is correct *for the level* as long as the mode is already set. The
app's level names in 1.38.0i (`ANC_LEVEL_NOISE_ADAPTIVE/ANTI_WIND/BALANCE/
COMFORTABLE/DEEP/DEEP_PLUS`, `ANC_LEVEL_TRANSPARENT_*`) show the level is model-
and firmware-specific, so treat it as an opaque 0–2 knob.

How this was pinned down: the earphone app's own code (1.38.0i,
`FunctionConfigImpl.updateNoise` → `SetDeviceConfigHelper.sendSetRunInfo`,
`VendorData{type=4, length=2, datas=[mode]}`), then verified on the wire — the
response is `status=ok` and config id 11 changes to the new mode without any
other frame.

Traps, all confirmed on the 6 Lite:

- Not reported anywhere else: `GET_DEVICE_RUN_INFO` attribute `0x0b` stays `00`
  in every mode, `GET_TARGET_INFO` attribute 4 also stays `00`, and no
  `NOTIFY_DEVICE_CONFIG` is sent. Poll config id 11 to read the mode.
- The app refuses to switch modes while the buds sit in the case (the taps do
  nothing), so test or script the change with the buds outside it.

### Custom equalizer (verified on the Buds 6 Lite, 2026-11-02)

Config id 55 carries a ten-band curve. It is a command channel rather than a
plain value: the first bytes say which sub-command the payload is. The layouts
come from the app's `DeviceConfigCustomEq` (reads: `valueToParams`, writes:
`paramsToValue`).

**Report** (what a GET returns):

```
01 | mode | maxBound | minBound | eqID | nameLen | name… | count
                                                  | band 0..count:
                                                    freq (u16 BE) | gain (u8)
```

Gains are symmetric: `0..127` is `0..+127`, `128 + n` is `-n` (`86` = −6).
The 6 Lite reports mode 10 (`Personalized`), bounds `06 06`, and ten bands:
62, 125, 250, 500, 1000, 2000, 4000, 8000, 12000, 16000 Hz. `nameLen` is 0;
the app skips that many bytes either way.

**Writes:**

| sub-command | payload | meaning |
|---|---|---|
| 1 `CMD_SET_CUSTOM_EQ` | `01 0a 01 01 01 00 <count>` + bands | store the curve |
| 5 `CMD_CUSTOM_EQ_PREVIEW` | `05 01 01 <count>` + bands | apply the curve |
| 4 `CMD_RESET_CUSTOM_EQ` | `04 01 01 01` | flat curve |
| 3 `CMD_SELECT_EQ_MODE` | `01 <mode>` | select preset (same as config id 7) |

The app hardcodes the mode byte 10 and the three `01`s in the set header; the
firmware echoes them back, so the report afterwards reads `… 01 00 …`.

Traps, all confirmed on the 6 Lite:

- **Sub-command 1 alone is inaudible.** The value reads back changed, so a
  write/read test passes, but the DSP keeps playing the old curve. Same
  "stored but not applied" trap as config id 11.
- **Sub-command 5 is the one that moves the audio path.** It also changes what
  config id 55 reports, so it is not a pure audition: treat 5 as "store and
  apply", and 1 as "store for the next preset load".
- The stored curve becomes audible when mode 10 is (re)selected, either after
  switching to another preset or from the phone app. Re-selecting 10 while it
  is already active is a no-op.
- The device stores gains outside the reported bound (`09` is echoed as `09`);
  the ±6 dB limit lives in the app's UI, not in the firmware.
- Send the bands in the reported order with the reported frequencies; the
  firmware neither sorts them nor fills in missing ones.

The desktop client therefore sends 1 then 5 for a normal edit, and 5 alone
while a GUI slider moves.

## Firmware capabilities, probed (2026-10-31)

Probed with `examples/probe.rs` against the real buds: write a value, read it
back, restore the original. Each change below was undone afterwards.

**How to tell what's supported.** Reads prove nothing: any unknown config id
answers with a one-byte `00`. Writes to unknown ids also answer `ok` and are
silently ignored. A feature is supported only when a written value **reads back
changed**. Some ids answer `not_supported` (9) or `param_error` (7) explicitly.
Config ids appear to be one byte inside the firmware: GET `0xEEEE` answers as id
`0xEE`.

Full id table from the app: `com.mi.earphone.bluetoothsdk.constant.DeviceConfigIdConstantKt`.
Which features the app *shows* is decided per model by a server-side function
list (`DeviceModel.hasFunction`), not by the firmware.

| id | feature | firmware response |
|---|---|---|
| 7 | EQ preset | values 0–13 are stored and read back, ≥14 → `param_error`. **Only 0, 1, 5, 6, 7 change the sound** (the app's set). Selecting 2–4 or 8–13 leaves the previous curve playing, not even falling back to a default (checked by ear, 2026-10-31), so the firmware accepts those ids but has no handling for them |
| 47 | low latency / game mode | supported: `01` reads back. Not hidden: the server's feature list for this model includes `FUNC_LOW_LATENCY` (3011) |
| 2 | gestures | four slots: single (4), double (1), triple (2), hold (3); type 0 aliases single tap. Actions 0–5 and 8 work. **9 = take photo** is accepted; on single tap or hold it couples the two slots of that bud (setting either sets both, clearing one resets the other to 0 = voice assistant), on double or triple tap it is independent. Actions 6, 7, 10–13 → `not_supported`; 14–255 are silently ignored (full sweep over types 0–15). **No swipe:** type 5 (slide) and types 6–15 are acknowledged but never stored, and a left-bud swipe set to volume (11) or volume up (4) did nothing when tested by hand |
| 4 | multipoint / dual connection | `not_supported` |
| 8 | device name (not in the app's table) | readable (`Redmi Buds 6 Play`), writes ignored |
| 55 | custom EQ (`01 0a 01 01 01 00 N [freq_be16 gain]…`) | ignored on the 6 Play (no custom EQ there; the 6 Lite implements it, see above) |
| 3, 5, 6, 10–13, 29, 36, 37, 41, 54, 58–60, 72, 89, 96, 97, 100, 102–104, 115, 122, 126, 161 | auto-answer, fit test, ANC, call listener, spatial, adaptive volume, … | ignored on the 6 Play (no ANC hardware); id 11 works on the 6 Lite (see above) |

### EQ preset ids across the product line

The app has no built-in preset list. Each model's list comes from Xiaomi's
product catalogue, as function `FUNC_SOUND_SETTINGS_SOUND_MODE_OTA` (2016),
whose `extra_data` is `{"sound_effect":{"effects":[…]}, "ota_sound_effect":[{"fw_version":…,"effects":[…]}]}`.
The catalogue needs no login:

```
POST https://<de|sg|ru|us|cn>.tws.wear.xiaomiwear.com/twswear/product/get_product_list
Cookie: auth_key=<fixed key compiled into the app's DeviceService>
data={"app_version":"1.38.0i","app_platform":0,"last_modify_time":0}
```

All five regions (about 40 models, including China-only ones), pulled 2026-10-31:

| id | English name in the app | used by |
|---|---|---|
| 0 | Balanced sound | most models |
| 1 | Enhance voice | all |
| 5 | Enhance bass | most Redmi models |
| 6 | Enhance treble | all |
| 7 | Boost volume | Redmi Buds 6 Play, 6 Active, 8 Active/Lite/8 |
| 10 | Personalized | Redmi Buds 5 Pro, 6 Lite, 6, 6 Pro, 8 Pro, Xiaomi Buds 5/6, … |
| 11 | Classic | Xiaomi Buds 4 Pro (fw ≥ 2.5.6.4), Xiaomi Buds 5 |
| 12 | Audiophile | Xiaomi Buds 4 Pro, Buds 5 Pro |
| 13 | Decrease bass | Xiaomi Buds 5, 5 Pro, 6 |
| 14 / 15 | Harman AudioEFX / Harman Master | Xiaomi Buds 5 (fw ≥ 3.0.7.6), 5 Pro, 6, OpenWear Stereo Pro |
| 16 / 17 / 18 | Standard / Outdoor / Underwater | one swim model (China catalogue) |
| 21 | (unnamed in 1.38.0i) | Xiaomi Buds 6, OpenWear Stereo Pro, REDMI Buds 8 family, Headphones Neo |
| **2, 3, 4, 8, 9** | — | **no model, in any region** |

Redmi Buds 6 Play (`n79bg`, PID 509b) gets `[0,6,5,1,7]`. Its closest sibling
Buds 6 Active (`n79g`) has the same features and the same EQ list. Buds 6 Lite
(`n79ag`) adds 3-level ANC and preset 10 (Personalized). Since the Lite shares
the chip family, 10 is the most plausible real extra on the Play. Slots 2–4,
8, 9 and 11–13 are stored by the Play's firmware, but no Xiaomi product names or
uses 2–4, 8 or 9; they are probably unused slots in the firmware's table.

Not probed (destructive or stateful): OTA (0xE1–E7), reboot (0x03), disconnect
(0x06), log upload (0xC1–C4), big-data transfer (52), recording (75), dongle
mode (77), codec headers (86, 119, 127), log mode (0xEEEE).

The buds also start their own `AUTH_CHECK` against the host in every session,
even when idle; the client answers it automatically.

Touches are not reported over RCSP: the actions go out as ordinary media/volume
commands over the audio profiles. What the buds *do* push unprompted is case
state. Taking a bud out of the case produced a battery frame
(`… e4 64 1e`: left 100 % charging, right 100 % not charging, case 30 %) and a
`NOTIFY_DEVICE_CONFIG` for id 12 (`REMIND_LOST`, value `06`, in-case bits).

## Ground truth: the whole captured session

The shape of the official app's session:

```
AUTH_CHECK  challenge →  AUTH_CHECK  transform(challenge)
AUTH_SEND_CALC_RESULT 01 00  →  01
GET_TARGET_INFO  ffffffff  →  name/version/battery/vid-pid/…
GET_DEVICE_RUN_INFO  ffffffff  →  addresses, status
SET_DEVICE_CONFIG  time=…  →  ok
GET_DEVICE_CONFIG  005d (93)  →  00
… EQ cycling: SET 7=00,01,05,06,07 each poked then read back
```

The EQ preset cycle in the capture is why preset values `00`–`07` are documented
as the observed range.

## Client

See `README.md`. The Rust crate (`src/`) is the main client: the library and CLI
have zero dependencies, and the RFCOMM socket is raw `AF_BLUETOOTH` FFI
(`src/bt.rs`). `gui/` is the GTK app. `python/redmi_buds.py` is the prototype the
Rust port was validated against. `examples/probe.rs` is the tool used for the
capability probing above.

## Known limitations

- The buds only serve the control channel to a host they are associated with.
  To use them from a desktop, disconnect them from the phone first.
