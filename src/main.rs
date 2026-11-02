//! `redmi-buds` — control Redmi/Xiaomi buds from a Linux desktop.
//!
//! See PROTOCOL.md for the wire format and xiaomi_auth.py for the reference
//! implementation of the auth transform that `src/auth.rs` ports.

use std::process::ExitCode;

use redmi_buds::auth;
use redmi_buds::client::{find_paired_buds, Buds};
use redmi_buds::rcsp;
use redmi_buds::settings::{self, EqBand, Gesture, NoiseMode};

const USAGE: &str = "\
redmi-buds - control Redmi/Xiaomi buds over Bluetooth RFCOMM

USAGE:
    redmi-buds [OPTIONS] <COMMAND>

OPTIONS:
    --mac <ADDR>      buds MAC address (default: first paired device named *Buds*)
    --channel <N>     RFCOMM channel (default: ask SDP, else probe)
    -v, --verbose     log the protocol exchange
    -h, --help        show this help

COMMANDS:
    info              device name, IDs and battery
    battery           battery levels
    eq get            read the EQ preset (config id 7)
    eq set <0-13>     set the EQ preset (0 balanced, 1 vocal, 5 bass,
                      6 treble, 7 volume, 10 personalized)
    eq custom get     show the custom curve (config id 55)
    eq custom set <-3,0,2,...> [--no-select]
                      one gain per band, or 62=-3,1000=2 for single bands;
                      stores, applies and selects the personalized preset
                      unless --no-select
    eq custom reset   back to a flat custom curve
    anc get           noise control mode and level (config id 11)
    anc set <MODE> [LEVEL]
                      MODE: off, noise, transparency; LEVEL 0-2 (default: keep)
    gesture get       show what each tap/hold does
    gesture set <single|double|triple|long> <left|right|both> <ACTION>
                      ACTION: none play prev next vol+ vol- assistant, or an id
    get <ID>...       read device config ids
    set <ID> <HEX>    write a raw config id / hex value
    find [--stop]     ring the buds (config id 9); --device 1|2|3
    selftest          verify the auth transform against hardware vectors
";

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Info,
    Battery,
    EqGet,
    EqSet(u8),
    EqCustomGet,
    EqCustomSet {
        /// `-3,0,2,…` (one gain per band) or `62=-3,1000=2`.
        gains: String,
        select: bool,
    },
    EqCustomReset,
    AncGet,
    AncSet(u8, Option<u8>),
    Get(Vec<u16>),
    Set(u16, Vec<u8>),
    Find { stop: bool, device: u8 },
    GestureGet,
    GestureSet(Gesture, Option<u8>, Option<u8>),
    Selftest,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
struct Opts {
    mac: Option<String>,
    channel: Option<u8>,
    verbose: bool,
    cmd: Cmd,
}

fn parse_u16(s: &str) -> Result<u16, String> {
    let s = s.trim();
    let v = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        s.parse::<u16>()
    };
    v.map_err(|_| format!("invalid id: {s}"))
}

fn parse_gain(s: &str) -> Option<i8> {
    s.trim().trim_start_matches('+').parse::<i8>().ok()
}

/// `-3,0,2` (one gain per band, same order as `eq custom get`) or
/// `62=-3,1000=2` (only the named bands).
fn apply_gains(bands: &[EqBand], arg: &str) -> Result<Vec<EqBand>, String> {
    let mut out = bands.to_vec();
    if arg.contains('=') {
        for part in arg.split(',') {
            let (freq, gain) = part
                .split_once('=')
                .ok_or_else(|| format!("bad band: {part}"))?;
            let freq: u16 = freq
                .trim()
                .trim_end_matches("Hz")
                .trim()
                .parse()
                .map_err(|_| format!("bad frequency: {freq}"))?;
            let gain = parse_gain(gain).ok_or_else(|| format!("bad gain: {gain}"))?;
            let band = out
                .iter_mut()
                .find(|b| b.freq == freq)
                .ok_or_else(|| format!("no band at {freq} Hz; see `eq custom get`"))?;
            band.gain = gain;
        }
    } else {
        let gains: Vec<i8> = arg
            .split(',')
            .map(parse_gain)
            .collect::<Option<Vec<i8>>>()
            .ok_or("gains must be numbers, e.g. -3,0,2")?;
        if gains.len() != out.len() {
            return Err(format!(
                "expected {} gains, got {}; see `eq custom get`",
                out.len(),
                gains.len()
            ));
        }
        for (band, gain) in out.iter_mut().zip(gains) {
            band.gain = gain;
        }
    }
    Ok(out)
}

fn parse_args() -> Result<Opts, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from<I, T>(args: I) -> Result<Opts, String>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let mut args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    if args.is_empty() {
        return Err("no command given".into());
    }

    let mut mac: Option<String> = None;
    let mut channel = None;
    let mut verbose = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mac" => {
                mac = Some(args.get(i + 1).ok_or("--mac needs a value")?.clone());
                i += 2;
            }
            "--channel" => {
                let v = args.get(i + 1).ok_or("--channel needs a value")?;
                channel = Some(v.parse::<u8>().map_err(|_| "bad --channel")?);
                i += 2;
            }
            "-v" | "--verbose" => {
                verbose = true;
                i += 1;
            }
            "-h" | "--help" => {
                return Ok(Opts {
                    mac,
                    channel,
                    verbose,
                    cmd: Cmd::Help,
                })
            }
            _ => break,
        }
    }
    args.drain(..i);
    if args.is_empty() {
        return Ok(Opts {
            mac,
            channel,
            verbose,
            cmd: Cmd::Help,
        });
    }

    let cmd = match args[0].as_str() {
        "info" => Cmd::Info,
        "battery" => Cmd::Battery,
        "selftest" => Cmd::Selftest,
        "eq" => match args.get(1).map(String::as_str) {
            Some("get") => Cmd::EqGet,
            Some("set") => {
                let v = args.get(2).ok_or("eq set needs a preset 0..13")?;
                let n = v.parse::<u8>().map_err(|_| "preset must be 0..13")?;
                if n > 13 {
                    return Err("preset must be 0..13".into());
                }
                Cmd::EqSet(n)
            }
            Some("custom") => {
                let usage = "usage: eq custom get | eq custom set <dB,...> [--no-select] \
                             | eq custom reset";
                match args.get(2).map(String::as_str) {
                    Some("get") => Cmd::EqCustomGet,
                    Some("reset") => Cmd::EqCustomReset,
                    Some("set") => Cmd::EqCustomSet {
                        gains: args.get(3).ok_or(usage)?.clone(),
                        select: !args.iter().any(|a| a == "--no-select"),
                    },
                    _ => return Err(usage.into()),
                }
            }
            _ => return Err("usage: eq get | eq set <0-13> | eq custom ...".into()),
        },
        "anc" => {
            let usage = "usage: anc get | anc set <off|noise|transparency> [level 0-2]";
            match args.get(1).map(String::as_str) {
                Some("get") => Cmd::AncGet,
                Some("set") => {
                    let mode = args
                        .get(2)
                        .and_then(|s| NoiseMode::parse(s))
                        .ok_or(usage)?;
                    let level = match args.get(3) {
                        Some(v) => {
                            let n = v.parse::<u8>().map_err(|_| "level must be 0-2")?;
                            if n > 2 {
                                return Err("level must be 0-2".into());
                            }
                            Some(n)
                        }
                        None => None,
                    };
                    Cmd::AncSet(mode as u8, level)
                }
                _ => return Err(usage.into()),
            }
        }
        "gesture" => match args.get(1).map(String::as_str) {
            Some("get") => Cmd::GestureGet,
            Some("set") => {
                let usage = "usage: gesture set <single|double|triple|long> <left|right|both> <ACTION>";
                let g = args.get(2).and_then(|s| Gesture::parse(s)).ok_or(usage)?;
                let side = args.get(3).ok_or(usage)?;
                let action = args
                    .get(4)
                    .and_then(|s| settings::parse_action(s))
                    .ok_or(usage)?;
                let (l, r) = match side.as_str() {
                    "left" => (Some(action), None),
                    "right" => (None, Some(action)),
                    "both" => (Some(action), Some(action)),
                    _ => return Err(usage.into()),
                };
                Cmd::GestureSet(g, l, r)
            }
            _ => return Err("usage: gesture get | gesture set ...".into()),
        },
        "get" => {
            let ids: Result<Vec<u16>, String> = args[1..].iter().map(|s| parse_u16(s)).collect();
            let ids = ids?;
            if ids.is_empty() {
                return Err("get needs at least one config id".into());
            }
            Cmd::Get(ids)
        }
        "set" => {
            let id = parse_u16(args.get(1).ok_or("set needs <ID> <HEX>")?)?;
            let hexval = args.get(2).ok_or("set needs <ID> <HEX>")?;
            let val = auth::hex_decode(hexval).ok_or("value must be hex")?;
            Cmd::Set(id, val)
        }
        "find" => {
            let mut stop = false;
            let mut device = 3u8;
            let mut j = 1;
            while j < args.len() {
                match args[j].as_str() {
                    "--stop" => stop = true,
                    "--device" => {
                        device = args
                            .get(j + 1)
                            .ok_or("--device needs a value")?
                            .parse()
                            .map_err(|_| "bad --device")?;
                        j += 1;
                    }
                    other => return Err(format!("unknown find option: {other}")),
                }
                j += 1;
            }
            Cmd::Find { stop, device }
        }
        other => return Err(format!("unknown command: {other}")),
    };

    Ok(Opts {
        mac,
        channel,
        verbose,
        cmd,
    })
}

fn fmt_battery(levels: &[(i16, bool)]) -> String {
    let names = ["L", "R", "case"];
    if levels.is_empty() {
        return "(unknown)".to_string();
    }
    levels
        .iter()
        .enumerate()
        .map(|(i, (pct, charging))| {
            let label = names.get(i).copied().unwrap_or("?");
            if *pct < 0 {
                format!("{label}=?")
            } else {
                format!("{label}={pct}%{}", if *charging { "⚡" } else { "" })
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn run(opts: &Opts) -> Result<(), String> {
    match opts.cmd {
        Cmd::Help => {
            print!("{USAGE}");
            return Ok(());
        }
        Cmd::Selftest => {
            let mut ok = true;
            println!(
                "auth transform vs {} recorded hardware responses:",
                auth::KNOWN_ANSWERS.len()
            );
            for (chal, want) in auth::KNOWN_ANSWERS.iter() {
                let c: [u8; 16] = auth::hex_decode(chal)
                    .and_then(|v| v.try_into().ok())
                    .ok_or("bad vector")?;
                let got = auth::hex_encode(&auth::transform(&auth::ADDR6_PLACEHOLDER, &c));
                let good = got == *want;
                ok &= good;
                println!(
                    "  {chal} -> {got}  [{}]",
                    if good { "ok" } else { "MISMATCH" }
                );
            }
            return if ok {
                Ok(())
            } else {
                Err("selftest failed".into())
            };
        }
        _ => {}
    }

    let mac = opts
        .mac
        .clone()
        .or_else(find_paired_buds)
        .ok_or("no paired buds found; pair them with bluetoothctl or pass --mac")?;
    let mut buds = Buds::connect(&mac, opts.channel, opts.verbose).map_err(|e| {
        format!(
            "cannot connect to {mac}: {e}\nhint: `bluetoothctl connect {mac}` first, and make sure \
             the buds are not held by the phone"
        )
    })?;

    buds.authenticate().map_err(|e| format!("auth failed: {e}"))?;
    eprintln!("authenticated");

    match &opts.cmd {
        Cmd::Info => {
            let info = buds.get_target_info().map_err(|e| e.to_string())?;
            if let Some(name) = &info.name {
                println!("name      {name}");
            }
            if let (Some(vid), Some(pid)) = (info.vid, info.pid) {
                println!("vendor    VID 0x{vid:04x}  PID 0x{pid:04x}");
            }
            if let Some(q) = info.quantity {
                println!("quantity  {q}");
            }
            let levels = if info.multi_battery.is_empty() {
                info.quantity
                    .map(|q| vec![(q as i16, false)])
                    .unwrap_or_default()
            } else {
                info.multi_battery.clone()
            };
            println!("battery   {}", fmt_battery(&levels));
            for (typ, val) in &info.raw {
                println!(
                    "  {:22} {}",
                    rcsp::target_attr_name(*typ),
                    auth::hex_encode(val)
                );
            }
        }
        Cmd::Battery => {
            let levels = buds.battery().map_err(|e| e.to_string())?;
            println!("{}", fmt_battery(&levels));
        }
        Cmd::AncGet => {
            let anc = buds.get_anc().map_err(|e| e.to_string())?;
            match anc {
                Some((mode, level)) => {
                    let name = NoiseMode::from_id(mode).map_or("<unknown>", NoiseMode::name);
                    let cycle = buds
                        .get_noise_cycle()
                        .ok()
                        .flatten()
                        .map(|(l, r)| {
                            let names = settings::noise_cycle_names(l.min(r));
                            names.join(" / ")
                        })
                        .unwrap_or_default();
                    println!("anc mode {mode} ({name})  level {level}");
                    if !cycle.is_empty() {
                        println!("long-press cycles: {cycle}");
                    }
                }
                None => println!("anc (absent)"),
            }
        }
        Cmd::AncSet(mode, level) => {
            buds.set_anc(*mode, *level).map_err(|e| e.to_string())?;
            let anc = buds.get_anc().map_err(|e| e.to_string())?;
            match anc {
                Some((m, l)) => println!(
                    "anc set to {m} ({}) level {l}",
                    NoiseMode::from_id(m).map_or("<unknown>", NoiseMode::name)
                ),
                None => println!("anc write sent"),
            }
        }
        Cmd::EqGet => {
            let preset = buds.get_eq().map_err(|e| e.to_string())?;
            match preset {
                Some(p) => println!(
                    "eq preset {p} ({})",
                    settings::eq_name(p).unwrap_or("unnamed")
                ),
                None => println!("eq preset (absent)"),
            }
        }
        Cmd::EqSet(n) => {
            buds.set_eq(*n).map_err(|e| e.to_string())?;
            println!("eq preset set to {n}");
        }
        Cmd::EqCustomGet => {
            let eq = buds.custom_eq().map_err(|e| e.to_string())?;
            match eq.filter(|e| !e.bands.is_empty()) {
                Some(eq) => {
                    println!(
                        "custom eq  {} bands, ±{} dB, mode {} ({})",
                        eq.bands.len(),
                        eq.bound,
                        eq.mode,
                        settings::eq_name(eq.mode).unwrap_or("unnamed")
                    );
                    for band in &eq.bands {
                        println!("  {:>8}  {:+}", band.label(), band.gain);
                    }
                }
                None => println!("custom eq (absent)"),
            }
        }
        Cmd::EqCustomSet { gains, select } => {
            let stored = buds.custom_eq().map_err(|e| e.to_string())?.unwrap_or_default();
            if stored.bands.is_empty() {
                return Err("these buds report no custom eq bands".into());
            }
            let bands = apply_gains(&stored.bands, gains)?;
            // Store, then apply: the set command alone is inaudible until the
            // personalized preset is (re)selected, preview moves the DSP now.
            buds.store_custom_eq(&bands)
                .map_err(|e| e.to_string())?;
            buds.preview_custom_eq(&bands)
                .map_err(|e| e.to_string())?;
            let selected = if *select {
                buds.set_eq(settings::EQ_PERSONALIZED)
                    .map_err(|e| e.to_string())?;
                true
            } else {
                false
            };
            println!(
                "saved {} bands{}",
                bands.len(),
                if selected {
                    format!(" (selected preset {})", settings::EQ_PERSONALIZED)
                } else {
                    String::new()
                }
            );
            for band in &bands {
                println!("  {:>8}  {:+}", band.label(), band.gain);
            }
        }
        Cmd::EqCustomReset => {
            buds.reset_custom_eq().map_err(|e| e.to_string())?;
            println!("custom eq reset");
        }
        Cmd::Get(ids) => {
            let cfg = buds.get_config(ids).map_err(|e| e.to_string())?;
            for id in ids {
                match cfg.iter().find(|(cid, _)| cid == id) {
                    Some((_, val)) => println!(
                        "{id:4} {:24} {}",
                        rcsp::config_id_name(*id),
                        auth::hex_encode(val)
                    ),
                    None => println!("{id:4} {:24} (absent)", rcsp::config_id_name(*id)),
                }
            }
        }
        Cmd::Set(id, val) => {
            buds.set_config(&[(*id, val.clone())])
                .map_err(|e| e.to_string())?;
            println!("config {id} set to {}", auth::hex_encode(val));
        }
        Cmd::Find { stop, device } => {
            buds.find_device(!stop, *device).map_err(|e| e.to_string())?;
            println!("{}", if *stop { "stopped" } else { "ringing" });
        }
        Cmd::GestureGet => {
            for b in buds.get_gestures().map_err(|e| e.to_string())? {
                println!(
                    "{:16} left: {:16} right: {}",
                    b.gesture.name(),
                    settings::action_name(b.left),
                    settings::action_name(b.right)
                );
            }
        }
        Cmd::GestureSet(g, l, r) => {
            buds.set_gesture(*g, *l, *r).map_err(|e| e.to_string())?;
            println!("{} updated", g.name());
        }
        Cmd::Selftest | Cmd::Help => unreachable!(),
    }
    Ok(())
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_anc_commands() {
        let opts = parse_args_from(["anc", "get"]).unwrap();
        assert_eq!(opts.cmd, Cmd::AncGet);

        let opts = parse_args_from(["anc", "set", "off"]).unwrap();
        assert_eq!(opts.cmd, Cmd::AncSet(0, None));

        let opts = parse_args_from(["anc", "set", "noise"]).unwrap();
        assert_eq!(opts.cmd, Cmd::AncSet(1, None));

        let opts = parse_args_from(["anc", "set", "transparency", "2"]).unwrap();
        assert_eq!(opts.cmd, Cmd::AncSet(2, Some(2)));

        let opts = parse_args_from(["anc", "set", "kapali"]).unwrap();
        assert_eq!(opts.cmd, Cmd::AncSet(0, None));

        // Rejection of invalid level (>2)
        let err = parse_args_from(["anc", "set", "noise", "3"]).unwrap_err();
        assert!(err.contains("level must be 0-2"));

        let err = parse_args_from(["anc", "set", "noise", "foo"]).unwrap_err();
        assert!(err.contains("level must be 0-2"));

        // Invalid mode
        let err = parse_args_from(["anc", "set", "unknown"]).unwrap_err();
        assert!(err.contains("usage: anc get | anc set"));
    }

    #[test]
    fn parse_eq_commands() {
        let opts = parse_args_from(["eq", "get"]).unwrap();
        assert_eq!(opts.cmd, Cmd::EqGet);

        let opts = parse_args_from(["eq", "set", "0"]).unwrap();
        assert_eq!(opts.cmd, Cmd::EqSet(0));

        let opts = parse_args_from(["eq", "set", "10"]).unwrap();
        assert_eq!(opts.cmd, Cmd::EqSet(10));

        let opts = parse_args_from(["eq", "set", "13"]).unwrap();
        assert_eq!(opts.cmd, Cmd::EqSet(13));

        // 14 is out of range
        let err = parse_args_from(["eq", "set", "14"]).unwrap_err();
        assert!(err.contains("preset must be 0..13"));
    }

    #[test]
    fn parse_flags_and_channel() {
        let opts = parse_args_from(["--channel", "29", "--verbose", "anc", "get"]).unwrap();
        assert_eq!(opts.channel, Some(29));
        assert!(opts.verbose);
        assert_eq!(opts.cmd, Cmd::AncGet);

        let opts = parse_args_from(["--mac", "11:22:33:44:55:66", "battery"]).unwrap();
        assert_eq!(opts.mac, Some("11:22:33:44:55:66".into()));
        assert_eq!(opts.cmd, Cmd::Battery);
    }
}
