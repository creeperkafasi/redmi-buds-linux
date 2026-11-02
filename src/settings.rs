//! Human-level meaning of the device-config values: EQ preset names and the
//! gesture table (config id 2).
//!
//! Names come from the earphone app: `SoundEffectExtKt.getSoundEffectName` for
//! presets, `GestureControlViewModel.getStringFromConfig` for gesture actions,
//! and `FunctionConfigImpl.gestureClickSet` / `DeviceConfigClickSet` for the
//! write encoding.

/// Config id of the EQ preset (`DeviceConfigSoundEffect`).
pub const CFG_EQ: u16 = 7;
/// Config id of the gesture table (`DeviceConfigClickSet`).
pub const CFG_GESTURES: u16 = 2;
/// Config id of the find-device ring (`DeviceConfigFindDevice`).
pub const CFG_FIND: u16 = 9;
/// Config id of the serial number.
pub const CFG_SERIAL: u16 = 39;
/// Config id of the noise-control mode and level (`DeviceConfigAncState`).
pub const CFG_ANC: u16 = 11;
/// Config id of the modes the long-press cycles through, one byte per side.
pub const CFG_NOISE_CYCLE: u16 = 10;

/// Attribute type of the noise-control mode in `SET_TARGET_INFO` (0x08).
///
/// The app's `FunctionConfigImpl.updateNoise` sends this attribute, not a
/// device-config write; see [`encode_anc_switch`].
pub const ATTR_ANC_STATUS: u8 = 4;

/// Noise-control mode, byte 0 of config id 11.
///
/// Verified against the earphone app 1.38.0i on a Redmi Buds 6 Lite: tapping
/// "Noise cancelling" ends up as `01`, "Transparency" as `02` and "Off" as
/// `00` — in the `SET_TARGET_INFO` attribute, which the firmware then mirrors
/// into config id 11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseMode {
    Off = 0,
    Cancelling = 1,
    Transparency = 2,
}

impl NoiseMode {
    pub const ALL: [NoiseMode; 3] = [
        NoiseMode::Off,
        NoiseMode::Cancelling,
        NoiseMode::Transparency,
    ];

    pub fn from_id(id: u8) -> Option<NoiseMode> {
        NoiseMode::ALL.into_iter().find(|m| *m as u8 == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            NoiseMode::Off => "Off",
            NoiseMode::Cancelling => "Noise cancelling",
            NoiseMode::Transparency => "Transparency",
        }
    }

    /// Short form for CLI arguments and toggle labels.
    pub fn parse(s: &str) -> Option<NoiseMode> {
        match s.to_ascii_lowercase().as_str() {
            "off" | "0" | "kapali" => Some(NoiseMode::Off),
            "anc" | "noise" | "cancelling" | "1" => Some(NoiseMode::Cancelling),
            "transparency" | "transparent" | "2" => Some(NoiseMode::Transparency),
            _ => None,
        }
    }
}

/// Decode config id 11: `[mode, level]`. Returns the raw mode, so unknown
/// values (the firmware stores 3 as well) survive a read.
pub fn decode_anc(value: &[u8]) -> Option<(u8, u8)> {
    Some((*value.first()?, value.get(1).copied().unwrap_or(0)))
}

/// Encode a mode write. The firmware forces the level to 0 while the mode is
/// off, so don't send a stale level with it.
pub fn encode_anc(mode: u8, level: u8) -> Vec<u8> {
    if mode == 0 {
        vec![0, 0]
    } else {
        vec![mode, level]
    }
}

/// Parameters of the `SET_TARGET_INFO` frame that switches the noise mode.
///
/// A vendor TLV of `len(u8) | type(u8) | value(len-1)`, i.e. `02 04 <mode>`.
/// This is the only thing that moves the buds' audio path: writing config id
/// 11 stores the same state (the app then shows it) without applying it.
pub fn encode_anc_switch(mode: u8) -> [u8; 3] {
    [0x02, ATTR_ANC_STATUS, mode]
}

/// Bit per selectable mode in config id 10 (the long-press cycle).
pub fn noise_cycle_names(mask: u8) -> Vec<&'static str> {
    NoiseMode::ALL
        .into_iter()
        .filter(|m| mask & (1 << (*m as u8)) != 0)
        .map(NoiseMode::name)
        .collect()
}

/// Config id of the custom equalizer (`DeviceConfigCustomEq`).
pub const CFG_CUSTOM_EQ: u16 = 55;

/// Preset id that makes the custom bands audible ("Personalized").
pub const EQ_PERSONALIZED: u8 = 10;

/// One band of the custom equalizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EqBand {
    pub freq: u16,
    pub gain: i8,
}

impl EqBand {
    /// Compact axis label: `62 Hz`, `1 kHz`, `12 kHz`.
    pub fn label(&self) -> String {
        if self.freq < 1000 {
            format!("{} Hz", self.freq)
        } else if self.freq % 1000 == 0 {
            format!("{} kHz", self.freq / 1000)
        } else {
            format!("{:.1} kHz", self.freq as f32 / 1000.0)
        }
    }
}

/// What the buds report about the custom curve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomEq {
    /// Sound-effect mode the stored curve belongs to (10 on the 6 Lite).
    pub mode: u8,
    /// The curve is limited to `-bound ..= bound` dB.
    pub bound: i8,
    pub bands: Vec<EqBand>,
}

/// Gain byte: negatives are stored as `128 + |gain|`.
fn gain_to_byte(gain: i8) -> u8 {
    if gain < 0 {
        (128 - gain as i16) as u8
    } else {
        gain as u8
    }
}

fn gain_from_byte(value: u8) -> i8 {
    if value < 128 {
        value as i8
    } else {
        (128 - value as i16) as i8
    }
}

/// Decode the `CMD_REPORT_EQ_PARAMS` value of config id 55.
///
/// Layout, from the app's `DeviceConfigCustomEq.valueToParams`:
/// `version | mode | bound | bound | eq_id | name_len | name… | count`, then
/// `count` × (`freq u16 BE` | `gain u8`).
pub fn decode_custom_eq(value: &[u8]) -> Option<CustomEq> {
    if value.len() < 7 || value[0] != 1 {
        return None;
    }
    let mode = value[1];
    let bound = if value[2] == 0 { 10 } else { value[2] as i8 };
    let mut at = 6 + value[5] as usize;
    let count = *value.get(at)? as usize;
    at += 1;
    let mut bands = Vec::new();
    // The app stops as soon as fewer than three bytes are left.
    while value.len() - at > 2 && bands.len() < count {
        let freq = u16::from_be_bytes([value[at], value[at + 1]]);
        bands.push(EqBand {
            freq,
            gain: gain_from_byte(value[at + 2]),
        });
        at += 3;
    }
    Some(CustomEq { mode, bound, bands })
}

/// Encode `CMD_SET_CUSTOM_EQ`: store the curve in the personalized slot.
pub fn encode_custom_eq(bands: &[EqBand]) -> Vec<u8> {
    // The header is the app's hardcoded `{1, 10, 1, 1, 1, 0, count}`.
    let mut out = vec![1, EQ_PERSONALIZED, 1, 1, 1, 0, bands.len() as u8];
    for band in bands {
        out.extend_from_slice(&band.freq.to_be_bytes());
        out.push(gain_to_byte(band.gain));
    }
    out
}

/// Encode `CMD_CUSTOM_EQ_PREVIEW`: hear the curve without storing it.
pub fn encode_custom_eq_preview(bands: &[EqBand]) -> Vec<u8> {
    let mut out = vec![5, 1, 1, bands.len() as u8];
    for band in bands {
        out.extend_from_slice(&band.freq.to_be_bytes());
        out.push(gain_to_byte(band.gain));
    }
    out
}

/// Encode `CMD_RESET_CUSTOM_EQ`: back to the flat curve.
pub fn encode_custom_eq_reset() -> Vec<u8> {
    vec![4, 1, 1, 1]
}

/// The presets the app offers for this model, in app order. The ids are the
/// values the capture shows the app writing to config id 7.
pub const EQ_PRESETS: [(u8, &str); 6] = [
    (0, "Balanced"),
    (1, "Vocal"),
    (5, "Bass boost"),
    (6, "Treble boost"),
    (7, "Volume boost"),
    (10, "Personalized"),
];

pub fn eq_name(id: u8) -> Option<&'static str> {
    EQ_PRESETS.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

/// Untouched-side marker in a gesture write.
pub const GESTURE_KEEP: u8 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Double = 1,
    Triple = 2,
    LongPress = 3,
    Single = 4,
    Slide = 5,
}

impl Gesture {
    pub const ALL: [Gesture; 5] = [
        Gesture::Single,
        Gesture::Double,
        Gesture::Triple,
        Gesture::LongPress,
        Gesture::Slide,
    ];

    pub fn from_id(id: u8) -> Option<Gesture> {
        Gesture::ALL.into_iter().find(|g| *g as u8 == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Gesture::Single => "Single tap",
            Gesture::Double => "Double tap",
            Gesture::Triple => "Triple tap",
            Gesture::LongPress => "Press and hold",
            Gesture::Slide => "Slide",
        }
    }

    pub fn parse(s: &str) -> Option<Gesture> {
        match s {
            "single" | "1x" => Some(Gesture::Single),
            "double" | "2x" => Some(Gesture::Double),
            "triple" | "3x" => Some(Gesture::Triple),
            "long" | "hold" => Some(Gesture::LongPress),
            "slide" => Some(Gesture::Slide),
            _ => None,
        }
    }
}

/// Gesture actions the app offers on music-only buds (no ANC, no camera).
pub const GESTURE_ACTIONS: [(u8, &str); 7] = [
    (8, "Nothing"),
    (1, "Play / pause"),
    (2, "Previous track"),
    (3, "Next track"),
    (4, "Volume up"),
    (5, "Volume down"),
    (0, "Voice assistant"),
];

pub fn action_name(id: u8) -> String {
    let known = match id {
        6 => Some("Noise control"),
        9 => Some("Take photo"),
        10 => Some("Record"),
        11 => Some("Volume"),
        12 => Some("Translate"),
        13 => Some("Previous / next"),
        _ => GESTURE_ACTIONS.iter().find(|(i, _)| *i == id).map(|(_, n)| *n),
    };
    known.map_or_else(|| format!("Action {id}"), str::to_string)
}

pub fn parse_action(s: &str) -> Option<u8> {
    let id = match s {
        "none" | "nothing" => 8,
        "play" | "pause" | "playpause" => 1,
        "prev" | "previous" => 2,
        "next" => 3,
        "vol+" | "volup" => 4,
        "vol-" | "voldown" => 5,
        "assistant" | "voice" => 0,
        n => n.parse().ok()?,
    };
    Some(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureBinding {
    pub gesture: Gesture,
    pub left: u8,
    pub right: u8,
}

/// Decode config id 2: repeated `[gesture, left, right]` triples.
pub fn decode_gestures(value: &[u8]) -> Vec<GestureBinding> {
    value
        .chunks_exact(3)
        .filter_map(|c| {
            Some(GestureBinding {
                gesture: Gesture::from_id(c[0])?,
                left: c[1],
                right: c[2],
            })
        })
        .collect()
}

/// Encode a single gesture write; `None` leaves that side unchanged.
pub fn encode_gesture(gesture: Gesture, left: Option<u8>, right: Option<u8>) -> Vec<u8> {
    vec![
        gesture as u8,
        left.unwrap_or(GESTURE_KEEP),
        right.unwrap_or(GESTURE_KEEP),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_observed_gesture_table() {
        // value read from the real buds
        let v = [4, 8, 4, 1, 1, 1, 2, 3, 3, 3, 8, 8];
        let g = decode_gestures(&v);
        assert_eq!(g.len(), 4);
        assert_eq!(
            g[0],
            GestureBinding { gesture: Gesture::Single, left: 8, right: 4 }
        );
        assert_eq!(g[1].gesture, Gesture::Double);
        assert_eq!(action_name(g[1].left), "Play / pause");
        assert_eq!(g[3].gesture, Gesture::LongPress);
    }

    #[test]
    fn gesture_write_marks_untouched_side() {
        assert_eq!(encode_gesture(Gesture::Double, Some(3), None), [1, 3, 0xFF]);
        assert_eq!(encode_gesture(Gesture::LongPress, None, Some(0)), [3, 0xFF, 0]);
    }

    #[test]
    fn anc_encoding() {
        assert_eq!(decode_anc(&[1, 0]), Some((1, 0)));
        assert_eq!(decode_anc(&[2]), Some((2, 0)));
        assert_eq!(decode_anc(&[0, 0]), Some((0, 0)));
        assert_eq!(decode_anc(&[3, 1]), Some((3, 1))); // Unknown modes preserved
        assert_eq!(decode_anc(&[]), None);

        // Level is meaningless while the mode is off.
        assert_eq!(encode_anc(0, 0), vec![0, 0]);
        assert_eq!(encode_anc(0, 2), vec![0, 0]);
        assert_eq!(encode_anc(1, 0), vec![1, 0]);
        assert_eq!(encode_anc(1, 2), vec![1, 2]);
        assert_eq!(encode_anc(2, 1), vec![2, 1]);

        // Verbatim from the app (FunctionConfigImpl.updateNoise): length 2,
        // type 4, the mode.
        assert_eq!(encode_anc_switch(0), [0x02, 0x04, 0x00]);
        assert_eq!(encode_anc_switch(1), [0x02, 0x04, 0x01]);
        assert_eq!(encode_anc_switch(2), [0x02, 0x04, 0x02]);
    }

    #[test]
    fn noise_mode_parsing_and_names() {
        assert_eq!(NoiseMode::parse("off"), Some(NoiseMode::Off));
        assert_eq!(NoiseMode::parse("0"), Some(NoiseMode::Off));
        assert_eq!(NoiseMode::parse("kapali"), Some(NoiseMode::Off));
        assert_eq!(NoiseMode::parse("OFF"), Some(NoiseMode::Off));
        assert_eq!(NoiseMode::parse("KAPALI"), Some(NoiseMode::Off));

        assert_eq!(NoiseMode::parse("anc"), Some(NoiseMode::Cancelling));
        assert_eq!(NoiseMode::parse("noise"), Some(NoiseMode::Cancelling));
        assert_eq!(NoiseMode::parse("cancelling"), Some(NoiseMode::Cancelling));
        assert_eq!(NoiseMode::parse("1"), Some(NoiseMode::Cancelling));
        assert_eq!(NoiseMode::parse("ANC"), Some(NoiseMode::Cancelling));

        assert_eq!(NoiseMode::parse("transparency"), Some(NoiseMode::Transparency));
        assert_eq!(NoiseMode::parse("transparent"), Some(NoiseMode::Transparency));
        assert_eq!(NoiseMode::parse("2"), Some(NoiseMode::Transparency));
        assert_eq!(NoiseMode::parse("TRANSPARENT"), Some(NoiseMode::Transparency));

        assert_eq!(NoiseMode::parse(""), None);
        assert_eq!(NoiseMode::parse("3"), None);
        assert_eq!(NoiseMode::parse("spatial"), None);

        assert_eq!(NoiseMode::from_id(0), Some(NoiseMode::Off));
        assert_eq!(NoiseMode::from_id(1), Some(NoiseMode::Cancelling));
        assert_eq!(NoiseMode::from_id(2), Some(NoiseMode::Transparency));
        assert_eq!(NoiseMode::from_id(3), None);
        assert_eq!(NoiseMode::from_id(5), None);

        assert_eq!(NoiseMode::Off.name(), "Off");
        assert_eq!(NoiseMode::Cancelling.name(), "Noise cancelling");
        assert_eq!(NoiseMode::Transparency.name(), "Transparency");
    }

    #[test]
    fn noise_cycle_masks() {
        // Buds 6 Lite reports 0x07 (all 3 modes supported)
        assert_eq!(
            noise_cycle_names(0x07),
            vec!["Off", "Noise cancelling", "Transparency"]
        );
        // Buds 6 Play reports 0x00 (no ANC supported)
        assert_eq!(noise_cycle_names(0x00), Vec::<&str>::new());

        assert_eq!(noise_cycle_names(0x01), vec!["Off"]);
        assert_eq!(noise_cycle_names(0x02), vec!["Noise cancelling"]);
        assert_eq!(noise_cycle_names(0x04), vec!["Transparency"]);
        assert_eq!(noise_cycle_names(0x03), vec!["Off", "Noise cancelling"]);
        // High bits outside 0..2 are ignored
        assert_eq!(noise_cycle_names(0xF8), Vec::<&str>::new());
    }

    #[test]
    fn custom_eq_round_trip() {
        // Verbatim read of config id 55 from a Redmi Buds 6 Lite.
        let raw = crate::auth::hex_decode(
            "010a060600000a003e06007d0600fa8601f40603e88607d0060fa0861f40862ee0863e8086",
        )
        .unwrap();
        let eq = decode_custom_eq(&raw).expect("decodes");
        assert_eq!(eq.mode, 10);
        assert_eq!(eq.bound, 6);
        assert_eq!(
            eq.bands,
            vec![
                EqBand { freq: 62, gain: 6 },
                EqBand { freq: 125, gain: 6 },
                EqBand { freq: 250, gain: -6 },
                EqBand { freq: 500, gain: 6 },
                EqBand { freq: 1000, gain: -6 },
                EqBand { freq: 2000, gain: 6 },
                EqBand { freq: 4000, gain: -6 },
                EqBand { freq: 8000, gain: -6 },
                EqBand { freq: 12000, gain: -6 },
                EqBand { freq: 16000, gain: -6 },
            ]
        );
        assert_eq!(eq.bands[0].label(), "62 Hz");
        assert_eq!(eq.bands[4].label(), "1 kHz");

        // The write header differs from the report: the app hardcodes
        // {1, 10, 1, 1, 1, 0, count} for CMD_SET_CUSTOM_EQ.
        let set = encode_custom_eq(&eq.bands);
        assert_eq!(&set[..7], &[1, 10, 1, 1, 1, 0, 10]);
        assert_eq!(&set[7..10], &[0x00, 0x3e, 0x06]);
        assert_eq!(set.len(), raw.len());
        assert_eq!(&set[7..], &raw[7..]);

        // Preview swaps the header and keeps the bands.
        let preview = encode_custom_eq_preview(&eq.bands);
        assert_eq!(&preview[..4], &[5, 1, 1, 10]);
        assert_eq!(&preview[4..], &raw[7..]);
        assert_eq!(encode_custom_eq_reset(), vec![4, 1, 1, 1]);
        assert_eq!(decode_custom_eq(&[1, 2, 3]), None);
    }

    #[test]
    fn names() {
        assert_eq!(eq_name(0), Some("Balanced"));
        assert_eq!(eq_name(5), Some("Bass boost"));
        assert_eq!(eq_name(10), Some("Personalized"));
        assert_eq!(eq_name(3), None);
        assert_eq!(action_name(42), "Action 42");
        assert_eq!(parse_action("next"), Some(3));
        assert_eq!(Gesture::parse("long"), Some(Gesture::LongPress));
    }
}
