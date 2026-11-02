//! The buds client: session auth plus the RCSP commands we use.

use std::collections::VecDeque;
use std::io;
use std::io::Read;
use std::process::Command;

use crate::auth;
use crate::bt::{parse_mac, Rfcomm};
use crate::rcsp::{self, Frame, APP_EARPHONE, APP_MIUI};
use crate::settings::{
    self, EqBand, Gesture, GestureBinding, CFG_ANC, CFG_CUSTOM_EQ, CFG_EQ, CFG_FIND, CFG_GESTURES,
    CFG_NOISE_CYCLE, CFG_SERIAL,
};

/// First paired device whose name looks like Xiaomi/Redmi buds, via `bluetoothctl`.
pub fn find_paired_buds() -> Option<String> {
    let out = Command::new("bluetoothctl")
        .args(["devices", "Paired"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let mut parts = l.splitn(3, ' ');
        (parts.next()? == "Device").then_some(())?;
        let mac = parts.next()?;
        let name = parts.next().unwrap_or("").to_lowercase();
        (name.contains("buds") || name.contains("airdots")).then(|| mac.to_string())
    })
}

/// RFCOMM channels to try when SDP is unavailable, most likely first.
///
/// The MIUI UUID (`0000fd2d-…`) sits on channel **6** on the Redmi Buds 6 Play
/// and on channel **29** on the Redmi Buds 6 Lite, so both have to be probed.
/// 1–5 are the plain-SPP channels some JieLi firmwares expose; they answer
/// immediately when absent.
///
/// Keep this list short: channels the firmware does not serve as RCSP can still
/// accept the RFCOMM connection (channel 12 does on the 6 Lite) and then sit
/// silent, so a wide sweep both wastes the connect timeout and can stop at a
/// channel that never answers the auth challenge.
const CHANNEL_CANDIDATES: [u8; 7] = [6, 29, 5, 4, 3, 2, 1];

const CONNECT_TIMEOUT_MS: i32 = 15_000;
const IO_TIMEOUT_MS: i32 = 8_000;

#[derive(Debug, Default)]
pub struct TargetInfo {
    pub name: Option<String>,
    pub quantity: Option<u8>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub multi_battery: Vec<(i16, bool)>,
    pub raw: Vec<(u8, Vec<u8>)>,
}

pub struct Buds {
    conn: Rfcomm,
    buf: Vec<u8>,
    pending: VecDeque<Frame>,
    sn: u8,
    verbose: bool,
    pub mac: [u8; 6],
    pub channel: u8,
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = [0u8; N];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Ask `sdptool` which channel serves the MIUI UUID.
///
/// `sdptool browse` prints one block per service record, with the RFCOMM channel
/// *after* the class-id/UUID lines, so the whole record has to be considered.
fn sdp_channel(mac: &str) -> Option<u8> {
    let out = Command::new("sdptool").arg("browse").arg(mac).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);

    fn channel_of(record: &[&str]) -> Option<u8> {
        if !record.iter().any(|l| l.contains("0000fd2d")) {
            return None;
        }
        record.iter().find_map(|l| {
            l.trim()
                .strip_prefix("Channel:")
                .and_then(|rest| rest.trim().parse::<u8>().ok())
        })
    }

    let mut record: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("Service Name:") && !record.is_empty() {
            if let Some(ch) = channel_of(&record) {
                return Some(ch);
            }
            record.clear();
        }
        record.push(line);
    }
    channel_of(&record)
}

impl Buds {
    pub fn connect(mac_str: &str, channel: Option<u8>, verbose: bool) -> io::Result<Self> {
        let mac = parse_mac(mac_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad MAC address"))?;

        let mut candidates: Vec<u8> = Vec::new();
        if let Some(ch) = channel {
            candidates.push(ch);
        } else {
            if let Some(ch) = sdp_channel(mac_str) {
                if verbose {
                    eprintln!("  SDP fd2d -> channel {ch}");
                }
                candidates.push(ch);
            }
            for ch in CHANNEL_CANDIDATES {
                if !candidates.contains(&ch) {
                    candidates.push(ch);
                }
            }
        }

        let mut last_err: Option<io::Error> = None;
        for ch in candidates {
            if verbose {
                eprintln!("  connecting RFCOMM {mac_str} channel {ch}");
            }
            match Rfcomm::connect(&mac, ch, CONNECT_TIMEOUT_MS) {
                Ok(conn) => {
                    if verbose {
                        eprintln!("  connected on channel {ch}");
                    }
                    return Ok(Buds {
                        conn,
                        buf: Vec::new(),
                        pending: VecDeque::new(),
                        sn: 0,
                        verbose,
                        mac,
                        channel: ch,
                    });
                }
                Err(e) => {
                    if verbose {
                        eprintln!("  channel {ch}: {e}");
                    }
                    // EHOSTDOWN / EHOSTUNREACH: the baseband page failed, so no
                    // other channel can succeed either. Don't wait out each one.
                    if matches!(e.raw_os_error(), Some(112) | Some(113)) {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no usable RFCOMM channel")
        }))
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            eprintln!("  {msg}");
        }
    }

    fn next_sn(&mut self) -> u8 {
        self.sn = self.sn.wrapping_add(1);
        self.sn
    }

    pub fn send_frame(&mut self, flags: u8, opcode: u8, payload: &[u8]) -> io::Result<()> {
        if self.verbose {
            eprintln!(
                "  -> [{} {} app={} sn=0x{:02x}] {}",
                if flags & 0x80 != 0 { "cmd" } else { "rsp" },
                rcsp::opcode_name(opcode),
                flags & 0x07,
                payload.first().copied().unwrap_or(0xFF),
                auth::hex_encode(payload)
            );
        }
        let raw = rcsp::encode(flags, opcode, payload);
        self.conn.send(&raw)
    }

    pub fn send_command(&mut self, opcode: u8, params: &[u8], app: u8) -> io::Result<u8> {
        let sn = self.next_sn();
        let mut payload = Vec::with_capacity(params.len() + 1);
        payload.push(sn);
        payload.extend_from_slice(params);
        self.send_frame(0x80 | 0x40 | app, opcode, &payload)?;
        Ok(sn)
    }

    /// Next complete frame, or `None` on timeout.
    pub fn recv_frame(&mut self, timeout_ms: i32) -> io::Result<Option<Frame>> {
        if let Some(f) = self.pending.pop_front() {
            self.log_frame(&f);
            return Ok(Some(f));
        }
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        let mut scratch = [0u8; 4096];
        loop {
            if let Some(f) = rcsp::extract(&mut self.buf) {
                self.log_frame(&f);
                return Ok(Some(f));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = remaining.as_millis() as i32;
            if wait <= 0 {
                return Ok(None);
            }
            match self.conn.recv(&mut scratch, wait)? {
                Some(n) if n > 0 => self.buf.extend_from_slice(&scratch[..n]),
                _ => {}
            }
        }
    }

    fn log_frame(&self, f: &Frame) {
        if self.verbose {
            let kind = if f.is_command() { "cmd" } else { "rsp" };
            let status = if f.is_command() {
                String::new()
            } else {
                format!(" status={}", rcsp::status_name(f.status()))
            };
            eprintln!(
                "  <- [{kind} {} app={} sn=0x{:02x}{status}] {}",
                f.opcode_name(),
                f.target_app(),
                f.sn(),
                auth::hex_encode(&f.payload)
            );
        }
    }

    /// Answer a challenge the buds initiates against us.
    fn answer_device_challenge(&mut self, f: &Frame) -> io::Result<bool> {
        if !(f.is_command() && f.opcode == 0x50) {
            return Ok(false);
        }
        let body = f.data();
        if body.len() < 17 || body[0] != 0x01 {
            return Ok(false);
        }
        let mut challenge = [0u8; 16];
        challenge.copy_from_slice(&body[1..17]);
        let resp = auth::transform(&auth::ADDR6_PLACEHOLDER, &challenge);
        let mut payload = vec![0x00, f.sn(), 0x01];
        payload.extend_from_slice(&resp);
        self.send_frame(APP_MIUI, 0x50, &payload)?;
        self.log("answered device-initiated challenge");
        Ok(true)
    }

    pub fn wait_response(&mut self, sn: u8, opcode: Option<u8>, timeout_ms: i32) -> io::Result<Frame> {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let wait = remaining.as_millis() as i32;
            if wait <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no response to sn=0x{sn:02x}"),
                ));
            }
            let Some(f) = self.recv_frame(wait)? else {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no response to sn=0x{sn:02x}"),
                ));
            };
            if self.answer_device_challenge(&f)? {
                continue;
            }
            if f.is_command() {
                continue;
            }
            if f.sn() == sn && opcode.is_none_or(|op| f.opcode == op) {
                return Ok(f);
            }
        }
    }

    pub fn request(&mut self, opcode: u8, params: &[u8], app: u8) -> io::Result<Frame> {
        let sn = self.send_command(opcode, params, app)?;
        let rsp = self.wait_response(sn, Some(opcode), IO_TIMEOUT_MS)?;
        if rsp.status() != 0 {
            return Err(io::Error::other(format!(
                "{} failed: {}",
                rcsp::opcode_name(opcode),
                rcsp::status_name(rsp.status())
            )));
        }
        Ok(rsp)
    }

    /// AUTH_CHECK challenge/response, then AUTH_SEND_CALC_RESULT.
    pub fn authenticate(&mut self) -> io::Result<()> {
        let challenge: [u8; 16] = random_bytes()?;
        self.log(&format!("challenge {}", auth::hex_encode(&challenge)));
        let expected = auth::transform(&auth::ADDR6_PLACEHOLDER, &challenge);

        let mut params = vec![0x01];
        params.extend_from_slice(&challenge);
        let sn = self.send_command(0x50, &params, APP_MIUI)?;
        let rsp = self.wait_response(sn, Some(0x50), IO_TIMEOUT_MS)?;
        if rsp.status() != 0 {
            return Err(io::Error::other(format!(
                "AUTH_CHECK rejected: {}",
                rcsp::status_name(rsp.status())
            )));
        }
        let data = rsp.data();
        if data.len() < 17 {
            return Err(io::Error::other("short AUTH_CHECK response"));
        }
        if data[1..17] != expected {
            return Err(io::Error::other(format!(
                "auth mismatch\n  device {}\n  local  {}",
                auth::hex_encode(&data[1..17]),
                auth::hex_encode(&expected)
            )));
        }
        self.log("challenge accepted by device");

        let sn = self.send_command(0x51, &[0x01, 0x00], APP_MIUI)?;
        let rsp = self.wait_response(sn, Some(0x51), IO_TIMEOUT_MS)?;
        if rsp.status() != 0 {
            return Err(io::Error::other(format!(
                "AUTH_SEND_CALC_RESULT rejected: {}",
                rcsp::status_name(rsp.status())
            )));
        }
        Ok(())
    }

    pub fn get_target_info(&mut self) -> io::Result<TargetInfo> {
        let rsp = self.request(0x02, &[0xFF, 0xFF, 0xFF, 0xFF], APP_EARPHONE)?;
        let mut info = TargetInfo::default();
        for (typ, val) in rcsp::parse_tlvs_u8(rsp.data()) {
            match typ {
                0 => info.name = Some(String::from_utf8_lossy(&val).into_owned()),
                2 => info.quantity = val.first().copied(),
                3 if val.len() >= 4 => {
                    info.vid = Some(u16::from_be_bytes([val[0], val[1]]));
                    info.pid = Some(u16::from_be_bytes([val[2], val[3]]));
                }
                7 => info.multi_battery = rcsp::decode_battery(&val),
                _ => info.raw.push((typ, val)),
            }
        }
        Ok(info)
    }

    pub fn battery(&mut self) -> io::Result<Vec<(i16, bool)>> {
        let info = self.get_target_info()?;
        if !info.multi_battery.is_empty() {
            return Ok(info.multi_battery);
        }
        Ok(info.quantity.map(|q| vec![(q as i16, false)]).unwrap_or_default())
    }

    pub fn get_config(&mut self, ids: &[u16]) -> io::Result<Vec<(u16, Vec<u8>)>> {
        let mut params = Vec::with_capacity(ids.len() * 2);
        for id in ids {
            params.extend_from_slice(&id.to_be_bytes());
        }
        let rsp = self.request(0xF3, &params, APP_EARPHONE)?;
        Ok(rcsp::parse_tlvs_u16(rsp.data()))
    }

    pub fn set_config(&mut self, items: &[(u16, Vec<u8>)]) -> io::Result<()> {
        let mut params = Vec::new();
        for (id, val) in items {
            params.extend_from_slice(&rcsp::tlv_u16(*id, val));
        }
        self.request(0xF2, &params, APP_EARPHONE)?;
        Ok(())
    }

    fn get_one(&mut self, id: u16) -> io::Result<Option<Vec<u8>>> {
        Ok(self
            .get_config(&[id])?
            .into_iter()
            .find(|(cid, _)| *cid == id)
            .map(|(_, v)| v))
    }

    pub fn get_eq(&mut self) -> io::Result<Option<u8>> {
        Ok(self.get_one(CFG_EQ)?.and_then(|v| v.first().copied()))
    }

    pub fn set_eq(&mut self, preset: u8) -> io::Result<()> {
        // The firmware stores 0..13, unnamed slots included, and the per-model
        // preset lists differ (the 6 Lite adds 10, "Personalized").
        if preset > 13 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "EQ preset must be 0..13",
            ));
        }
        self.set_config(&[(CFG_EQ, vec![preset])])
    }

    pub fn get_gestures(&mut self) -> io::Result<Vec<GestureBinding>> {
        Ok(self
            .get_one(CFG_GESTURES)?
            .map(|v| settings::decode_gestures(&v))
            .unwrap_or_default())
    }

    /// Rebind one gesture; `None` leaves that side as it is.
    pub fn set_gesture(&mut self, gesture: Gesture, left: Option<u8>, right: Option<u8>) -> io::Result<()> {
        let value = settings::encode_gesture(gesture, left, right);
        self.set_config(&[(CFG_GESTURES, value)])
    }

    /// Noise-control mode and level (config id 11), as stored by the buds.
    pub fn get_anc(&mut self) -> io::Result<Option<(u8, u8)>> {
        Ok(self.get_one(CFG_ANC)?.as_deref().and_then(settings::decode_anc))
    }

    /// Switch the noise-control mode. `level` is kept as it is when `None`.
    pub fn set_anc(&mut self, mode: u8, level: Option<u8>) -> io::Result<()> {
        // The audio path only moves on SET_TARGET_INFO attribute 4, which is
        // what the earphone app sends. A config id 11 write is stored (and the
        // app then reports the new mode) but never reaches the DSP, so it
        // looks like it worked while nothing is audible.
        let params = settings::encode_anc_switch(mode);
        self.request(0x08, &params, APP_EARPHONE)?;
        // The level has no attribute of its own: the app keeps it in config
        // id 11, and the firmware mirrors the mode it just got there.
        if let Some(level) = level {
            self.set_config(&[(CFG_ANC, settings::encode_anc(mode, level))])?;
        }
        Ok(())
    }

    /// Modes the long-press cycles through, per side (config id 10 bitmask).
    pub fn get_noise_cycle(&mut self) -> io::Result<Option<(u8, u8)>> {
        Ok(self
            .get_one(CFG_NOISE_CYCLE)?
            .filter(|v| v.len() >= 2)
            .map(|v| (v[0], v[1])))
    }

    /// The custom equalizer curve (config id 55), as the buds report it.
    pub fn custom_eq(&mut self) -> io::Result<Option<settings::CustomEq>> {
        Ok(self
            .get_one(CFG_CUSTOM_EQ)?
            .as_deref()
            .and_then(settings::decode_custom_eq))
    }

    /// Store the curve without applying it.
    ///
    /// `CMD_SET_CUSTOM_EQ` only persists the bands: the sound does not change
    /// until the curve is re-applied, either by re-selecting the personalized
    /// preset (10) or by the preview command below.
    pub fn store_custom_eq(&mut self, bands: &[EqBand]) -> io::Result<()> {
        self.set_config(&[(CFG_CUSTOM_EQ, settings::encode_custom_eq(bands))])
    }

    /// Apply the curve now.
    ///
    /// `CMD_CUSTOM_EQ_PREVIEW` is what the app sends while a slider moves. On
    /// the 6 Lite it is also the only command that moves the audio path right
    /// away, and the firmware updates the config 55 report with it as well.
    pub fn preview_custom_eq(&mut self, bands: &[EqBand]) -> io::Result<()> {
        self.set_config(&[(CFG_CUSTOM_EQ, settings::encode_custom_eq_preview(bands))])
    }

    /// Flat custom curve.
    ///
    /// The reset command only stores the flat curve, so the reported bands are
    /// previewed afterwards to make the change audible immediately.
    pub fn reset_custom_eq(&mut self) -> io::Result<()> {
        self.set_config(&[(CFG_CUSTOM_EQ, settings::encode_custom_eq_reset())])?;
        if let Some(eq) = self.custom_eq()? {
            if !eq.bands.is_empty() {
                let preview = settings::encode_custom_eq_preview(&eq.bands);
                self.set_config(&[(CFG_CUSTOM_EQ, preview)])?;
            }
        }
        Ok(())
    }

    pub fn serial(&mut self) -> io::Result<Option<String>> {
        Ok(self
            .get_one(CFG_SERIAL)?
            .map(|v| String::from_utf8_lossy(&v).into_owned()))
    }

    /// Ring the buds. Device config id 9 carries `[state, device_type]`.
    pub fn find_device(&mut self, ring: bool, device_type: u8) -> io::Result<()> {
        self.set_config(&[(CFG_FIND, vec![u8::from(ring), device_type])])
    }

    pub fn close(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDevice {
        conn: Rfcomm,
        buf: Vec<u8>,
    }

    impl MockDevice {
        fn new(conn: Rfcomm) -> Self {
            Self {
                conn,
                buf: Vec::with_capacity(1024),
            }
        }

        fn recv_cmd(&mut self) -> Frame {
            let mut scratch = [0u8; 1024];
            loop {
                if let Some(f) = rcsp::extract(&mut self.buf) {
                    return f;
                }
                let n = self.conn.recv(&mut scratch, 1000).unwrap().expect("eof");
                self.buf.extend_from_slice(&scratch[..n]);
            }
        }

        fn send_rsp(&mut self, cmd: &Frame, status: u8, data: &[u8]) {
            let mut payload = Vec::with_capacity(data.len() + 2);
            payload.push(status);
            payload.push(cmd.sn());
            payload.extend_from_slice(data);
            let raw = rcsp::encode(cmd.target_app(), cmd.opcode, &payload);
            self.conn.send(&raw).unwrap();
        }
    }

    impl Buds {
        fn new_mock(conn: Rfcomm) -> Self {
            Buds {
                conn,
                buf: Vec::with_capacity(1024),
                pending: VecDeque::new(),
                sn: 0,
                verbose: false,
                mac: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
                channel: 29,
            }
        }
    }

    #[test]
    fn channel_candidates_includes_buds_6_lite() {
        assert_eq!(CHANNEL_CANDIDATES[0], 6);
        assert_eq!(CHANNEL_CANDIDATES[1], 29);
        assert!(CHANNEL_CANDIDATES.contains(&6));
        assert!(CHANNEL_CANDIDATES.contains(&29));
    }

    #[test]
    fn get_anc_buds_6_lite() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            assert_eq!(cmd.opcode, 0xF3);
            assert_eq!(cmd.target_app(), APP_EARPHONE);
            assert_eq!(cmd.data(), &[0x00, 0x0B]); // CFG_ANC = 11
            let tlv = rcsp::tlv_u16(CFG_ANC, &[0x01, 0x02]); // Cancelling, level 2
            dev.send_rsp(&cmd, 0, &tlv);
        });

        let anc = buds.get_anc().unwrap();
        assert_eq!(anc, Some((1, 2)));
        handle.join().unwrap();
    }

    #[test]
    fn get_anc_buds_6_play() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            assert_eq!(cmd.opcode, 0xF3);
            let tlv = rcsp::tlv_u16(CFG_ANC, &[0x00, 0x00]);
            dev.send_rsp(&cmd, 0, &tlv);
        });

        let anc = buds.get_anc().unwrap();
        assert_eq!(anc, Some((0, 0)));
        handle.join().unwrap();
    }

    #[test]
    fn get_anc_absent() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            dev.send_rsp(&cmd, 0, &[]);
        });

        let anc = buds.get_anc().unwrap();
        assert_eq!(anc, None);
        handle.join().unwrap();
    }

    #[test]
    fn set_anc_mode_only() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            assert_eq!(cmd.opcode, 0x08); // SET_TARGET_INFO
            assert_eq!(cmd.target_app(), APP_EARPHONE);
            assert_eq!(cmd.data(), &[0x02, 0x04, 0x01]); // mode 1
            dev.send_rsp(&cmd, 0, &[]);
        });

        buds.set_anc(1, None).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn set_anc_mode_and_level() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd1 = dev.recv_cmd();
            assert_eq!(cmd1.opcode, 0x08);
            assert_eq!(cmd1.data(), &[0x02, 0x04, 0x02]); // Transparency = 2
            dev.send_rsp(&cmd1, 0, &[]);

            let cmd2 = dev.recv_cmd();
            assert_eq!(cmd2.opcode, 0xF2); // SET_DEVICE_CONFIG
            assert_eq!(cmd2.target_app(), APP_EARPHONE);
            let tlvs = rcsp::parse_tlvs_u16(cmd2.data());
            assert_eq!(tlvs.len(), 1);
            assert_eq!(tlvs[0], (CFG_ANC, vec![2, 1]));
            dev.send_rsp(&cmd2, 0, &[]);
        });

        buds.set_anc(2, Some(1)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn set_anc_off_forces_level_zero() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd1 = dev.recv_cmd();
            assert_eq!(cmd1.opcode, 0x08);
            assert_eq!(cmd1.data(), &[0x02, 0x04, 0x00]); // Off = 0
            dev.send_rsp(&cmd1, 0, &[]);

            let cmd2 = dev.recv_cmd();
            assert_eq!(cmd2.opcode, 0xF2);
            let tlvs = rcsp::parse_tlvs_u16(cmd2.data());
            assert_eq!(tlvs[0], (CFG_ANC, vec![0, 0]));
            dev.send_rsp(&cmd2, 0, &[]);
        });

        buds.set_anc(0, Some(2)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn get_noise_cycle_buds_6_lite_and_6_play() {
        // Buds 6 Lite: reports 0x07 0x07
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            assert_eq!(cmd.opcode, 0xF3);
            assert_eq!(cmd.data(), &[0x00, 0x0A]);
            let tlv = rcsp::tlv_u16(CFG_NOISE_CYCLE, &[0x07, 0x07]);
            dev.send_rsp(&cmd, 0, &tlv);
        });
        let cycle = buds.get_noise_cycle().unwrap();
        assert_eq!(cycle, Some((0x07, 0x07)));
        handle.join().unwrap();

        // Buds 6 Play: reports 0x00 0x00
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            let tlv = rcsp::tlv_u16(CFG_NOISE_CYCLE, &[0x00, 0x00]);
            dev.send_rsp(&cmd, 0, &tlv);
        });
        let cycle = buds.get_noise_cycle().unwrap();
        assert_eq!(cycle, Some((0x00, 0x00)));
        handle.join().unwrap();
    }

    #[test]
    fn set_anc_device_error_reporting() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            dev.send_rsp(&cmd, 9, &[]); // 9 = not_supported
        });

        let err = buds.set_anc(1, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SET_TARGET_INFO") && msg.contains("not_supported"),
            "unexpected error message: {msg}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn authentication_handshake() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd1 = dev.recv_cmd();
            assert_eq!(cmd1.opcode, 0x50);
            assert_eq!(cmd1.target_app(), APP_MIUI);
            let params = cmd1.data();
            assert_eq!(params[0], 0x01);
            let challenge: [u8; 16] = params[1..17].try_into().unwrap();
            let expected = auth::transform(&auth::ADDR6_PLACEHOLDER, &challenge);
            let mut rsp_data = vec![0x01];
            rsp_data.extend_from_slice(&expected);
            dev.send_rsp(&cmd1, 0, &rsp_data);

            let cmd2 = dev.recv_cmd();
            assert_eq!(cmd2.opcode, 0x51);
            assert_eq!(cmd2.target_app(), APP_MIUI);
            dev.send_rsp(&cmd2, 0, &[]);
        });

        buds.authenticate().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn set_eq_allows_preset_10_and_rejects_above_13() {
        let (c_rf, s_rf) = Rfcomm::pair().unwrap();
        let mut buds = Buds::new_mock(c_rf);
        let handle = std::thread::spawn(move || {
            let mut dev = MockDevice::new(s_rf);
            let cmd = dev.recv_cmd();
            assert_eq!(cmd.opcode, 0xF2);
            let tlvs = rcsp::parse_tlvs_u16(cmd.data());
            assert_eq!(tlvs[0], (CFG_EQ, vec![10]));
            dev.send_rsp(&cmd, 0, &[]);
        });

        buds.set_eq(10).unwrap();
        handle.join().unwrap();

        let err = buds.set_eq(14).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
