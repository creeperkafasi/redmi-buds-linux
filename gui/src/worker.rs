//! Background thread that owns the Bluetooth session.
//!
//! The RCSP client is blocking, so it lives on its own thread. The UI sends
//! [`Req`]s over a std channel and receives [`Ev`]s over an async channel.
//! While connected, battery is polled; while disconnected, the worker retries.

use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use redmi_buds::client::Buds;
use redmi_buds::settings::{Gesture, GestureBinding};

const POLL: Duration = Duration::from_secs(20);
const RETRY: Duration = Duration::from_secs(8);

pub enum Req {
    /// User asked to connect: also ask BlueZ to bring the link up first.
    Connect,
    Refresh,
    SetEq(u8),
    SetAnc(u8),
    SetGesture(Gesture, Option<u8>, Option<u8>),
    Find { ring: bool, device: u8 },
}

#[derive(Default, Clone)]
pub struct Snapshot {
    pub name: String,
    pub battery: Vec<(i16, bool)>,
    pub eq: Option<u8>,
    /// Noise-control mode and level (config id 11).
    pub anc: Option<(u8, u8)>,
    pub has_anc: bool,
    pub gestures: Vec<GestureBinding>,
    pub serial: Option<String>,
    pub mac: String,
}

pub enum Ev {
    Connecting,
    Connected(Snapshot),
    Battery(Vec<(i16, bool)>),
    Gestures(Vec<GestureBinding>),
    Eq(Option<u8>),
    Anc(Option<(u8, u8)>),
    /// Something the user did failed; the session is still fine.
    Failed(String),
    Disconnected(String),
}

fn snapshot(buds: &mut Buds, mac: &str) -> std::io::Result<Snapshot> {
    let info = buds.get_target_info()?;
    let battery = if info.multi_battery.is_empty() {
        info.quantity.map(|q| vec![(q as i16, false)]).unwrap_or_default()
    } else {
        info.multi_battery
    };
    let noise_cycle = buds.get_noise_cycle().unwrap_or(None);
    let has_anc = noise_cycle.map(|(l, r)| l != 0 || r != 0).unwrap_or(false);
    Ok(Snapshot {
        name: info.name.unwrap_or_else(|| "Buds".into()),
        battery,
        eq: buds.get_eq()?,
        anc: buds.get_anc()?,
        has_anc,
        gestures: buds.get_gestures()?,
        serial: buds.serial()?,
        mac: mac.to_string(),
    })
}

fn open(mac: &str) -> std::io::Result<Buds> {
    // Ask BlueZ to connect the buds first, exactly like the desktop's own
    // Bluetooth menu does. Opening the RFCOMM socket alone does bring the ACL
    // link up, but without the audio profiles: PipeWire never creates a card
    // and the buds look "connected" with no sound device (the same call the
    // earphone app makes on Android, where the system handles the profiles).
    let _ = Command::new("bluetoothctl")
        .args(["--timeout", "15", "connect", mac])
        .output();

    let mut buds = Buds::connect(mac, None, false)?;
    buds.authenticate()?;
    Ok(buds)
}

fn friendly(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(112) | Some(113) => {
            "The buds are not reachable. Take them out of the case, and turn off \
             Bluetooth on your phone if it is holding them."
                .into()
        }
        Some(111) => "The buds refused the connection.".into(),
        _ => e.to_string(),
    }
}

pub fn run(mac: String, reqs: Receiver<Req>, events: async_channel::Sender<Ev>) {
    let send = |ev: Ev| events.send_blocking(ev).is_ok();
    let mut session: Option<Buds> = None;
    let mut want_connect = true;

    loop {
        let Some(buds) = session.as_mut() else {
            if want_connect {
                if !send(Ev::Connecting) {
                    return;
                }
                match open(&mac).and_then(|mut b| snapshot(&mut b, &mac).map(|s| (b, s))) {
                    Ok((b, snap)) => {
                        session = Some(b);
                        if !send(Ev::Connected(snap)) {
                            return;
                        }
                        continue;
                    }
                    Err(e) => {
                        if !send(Ev::Disconnected(friendly(&e))) {
                            return;
                        }
                    }
                }
            }
            match reqs.recv_timeout(RETRY) {
                Ok(Req::Connect) => {
                    // Pull the link over from the phone if BlueZ can.
                    let _ = Command::new("bluetoothctl").args(["connect", &mac]).output();
                    want_connect = true;
                }
                Ok(_) => want_connect = true,
                Err(RecvTimeoutError::Timeout) => want_connect = true,
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        };

        let req = match reqs.recv_timeout(POLL) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => Req::Refresh,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        let result: std::io::Result<Option<Ev>> = match req {
            Req::Connect | Req::Refresh => buds.battery().map(|b| Some(Ev::Battery(b))),
            Req::SetEq(p) => buds
                .set_eq(p)
                .and_then(|_| buds.get_eq())
                .map(|e| Some(Ev::Eq(e))),
            Req::SetAnc(mode) => buds
                .set_anc(mode, None)
                .and_then(|_| buds.get_anc())
                .map(|a| Some(Ev::Anc(a))),
            Req::SetGesture(g, l, r) => buds
                .set_gesture(g, l, r)
                .and_then(|_| buds.get_gestures())
                .map(|g| Some(Ev::Gestures(g))),
            Req::Find { ring, device } => buds.find_device(ring, device).map(|_| None),
        };

        let ok = match result {
            Ok(Some(ev)) => send(ev),
            Ok(None) => true,
            // A rejected command keeps the session; a dead socket does not.
            Err(e) if e.kind() == std::io::ErrorKind::Other || e.kind() == std::io::ErrorKind::InvalidInput => {
                send(Ev::Failed(e.to_string()))
            }
            Err(e) => {
                session = None;
                want_connect = true;
                send(Ev::Disconnected(friendly(&e)))
            }
        };
        if !ok {
            return;
        }
    }
}
