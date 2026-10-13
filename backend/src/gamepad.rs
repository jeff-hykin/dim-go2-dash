// The handheld's own sticks, read here (Linux) instead of through the browser, so driving doesn't hang on Steam Input.
// On SteamOS, InputPlumber hides the Legion Go S's pad (1a86:e310) and re-presents it as a Steam-Deck-format HID pad
// (uhid, 28de:12ff, the "Legion Go S Controller" in Steam's controller.txt) that Steam turns into its virtual Xbox pad
// only for a signed-in profile. That hidraw node is the user's (uaccess) and every reader gets every report, so this
// reads it alongside Steam (no grab) and the page drives from it (api/drive/gamepad.ws), its Gamepad API the fallback.

use std::sync::OnceLock;

use serde::Serialize;
use tokio::sync::watch;

/// a Steam Deck input report: 01 00 09 40, then 60 bytes of state (InputPlumber drivers/steam_deck/hid_report.rs)
pub const REPORT_LEN: usize = 64;
const HEADER: [u8; 4] = [0x01, 0x00, 0x09, 0x40];
const FULL: f32 = 32767.0;
/// Valve's VID; 0x1205 the Steam Deck, 0x12f0..=0x12ff InputPlumber's Deck-format pads (0x12ff the Legion Go S)
const VALVE: u16 = 0x28de;

/// One pad state in the Gamepad API's standard mapping: axes LX LY RX RY (-1..1, down = +), 17 buttons (pressed, 0..1).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pad {
    pub id: String,
    pub axes: [f32; 4],
    pub buttons: Vec<(bool, f32)>,
}

/// (byte, mask) of each standard-mapping button: A B X Y LB RB LT RT View Menu L3 R3 Up Down Left Right Steam
const BUTTONS: [(usize, u8); 17] = [
    (8, 0x80),
    (8, 0x20),
    (8, 0x40),
    (8, 0x10),
    (8, 0x08),
    (8, 0x04),
    (8, 0x02),
    (8, 0x01),
    (9, 0x10),
    (9, 0x40),
    (10, 0x40),
    (11, 0x04),
    (9, 0x01),
    (9, 0x08),
    (9, 0x04),
    (9, 0x02),
    (9, 0x20),
];

fn round4(value: f32) -> f32 {
    (value * 10000.0).round() / 10000.0
}

fn i16_at(report: &[u8], at: usize) -> f32 {
    i16::from_le_bytes([report[at], report[at + 1]]) as f32
}

/// A Deck-format input report as a standard-mapping pad; None for any other report.
pub fn parse_report(report: &[u8], id: &str) -> Option<Pad> {
    if report.len() < REPORT_LEN || report[..4] != HEADER {
        return None;
    }
    let stick = |at: usize, sign: f32| round4((sign * i16_at(report, at) / FULL).clamp(-1.0, 1.0)) + 0.0;
    let trigger = |at: usize| round4((u16::from_le_bytes([report[at], report[at + 1]]) as f32 / FULL).clamp(0.0, 1.0));
    // the report's Y is up = +; the Gamepad API's is down = +
    let axes = [stick(48, 1.0), stick(50, -1.0), stick(52, 1.0), stick(54, -1.0)];
    let buttons = BUTTONS
        .iter()
        .enumerate()
        .map(|(index, &(byte, mask))| {
            let pressed = report[byte] & mask != 0;
            match index {
                6 => (pressed, trigger(44)),
                7 => (pressed, trigger(46)),
                _ => (pressed, if pressed { 1.0 } else { 0.0 }),
            }
        })
        .collect();
    Some(Pad { id: id.to_string(), axes, buttons })
}

/// A hidraw node's uevent (HID_ID=0003:000028DE:000012FF) is a Deck-format pad's: Valve's VID, a Deck-format PID.
pub fn is_deck_format(uevent: &str) -> bool {
    let Some(id) = uevent.lines().find_map(|line| line.strip_prefix("HID_ID=")) else {
        return false;
    };
    let mut parts = id.split(':').skip(1).map(|part| u32::from_str_radix(part, 16).ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(vendor), Some(product)) => vendor == VALVE as u32 && (product == 0x1205 || (0x12f0..=0x12ff).contains(&product)),
        _ => false,
    }
}

static PAD: OnceLock<watch::Sender<Option<Pad>>> = OnceLock::new();

/// The direct pad's state (None: there isn't one); the reader starts with the first subscriber (Linux only).
pub fn subscribe() -> watch::Receiver<Option<Pad>> {
    PAD.get_or_init(|| {
        let (sender, _) = watch::channel(None);
        #[cfg(target_os = "linux")]
        linux::start(sender.clone());
        sender
    })
    .subscribe()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::sync::watch;

    use super::{is_deck_format, parse_report, Pad, REPORT_LEN};

    /// InputPlumber writes every 4 ms, idle or not: this long without a report and the pad counts as gone
    const STALE_MS: i32 = 250;
    const RESCAN: Duration = Duration::from_secs(2);

    /// The Deck-format pad's hidraw node and its name, if there is one (its vendor-defined interface, not a keyboard's).
    fn find() -> Option<(PathBuf, String)> {
        let mut nodes: Vec<_> = std::fs::read_dir("/sys/class/hidraw").ok()?.flatten().map(|entry| entry.path()).collect();
        nodes.sort();
        nodes.into_iter().find_map(|node| {
            let uevent = std::fs::read_to_string(node.join("device/uevent")).ok()?;
            let descriptor = std::fs::read(node.join("device/report_descriptor")).ok()?;
            if !is_deck_format(&uevent) || descriptor.get(..3) != Some(&[0x06, 0xff, 0xff]) {
                return None;
            }
            let name = uevent.lines().find_map(|line| line.strip_prefix("HID_NAME=")).unwrap_or("Steam Deck format pad");
            Some((PathBuf::from("/dev").join(node.file_name()?), format!("{name} (direct)")))
        })
    }

    /// Reports until the node fails; None whenever none arrives for STALE_MS.
    fn read(file: &mut File, id: &str, sender: &watch::Sender<Option<Pad>>) {
        let mut report = [0u8; REPORT_LEN];
        loop {
            let mut fd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut fd, 1, STALE_MS) } <= 0 {
                sender.send_if_modified(|pad| pad.take().is_some());
                continue;
            }
            match file.read(&mut report) {
                Ok(length) if length > 0 => {
                    if let Some(next) = parse_report(&report[..length], id) {
                        sender.send_if_modified(|pad| {
                            if pad.as_ref() == Some(&next) {
                                return false;
                            }
                            *pad = Some(next);
                            true
                        });
                    }
                }
                _ => return,
            }
        }
    }

    pub fn start(sender: watch::Sender<Option<Pad>>) {
        let spawned = std::thread::Builder::new().name("gamepad".into()).spawn(move || loop {
            if let Some((path, id)) = find() {
                match File::open(&path) {
                    Ok(mut file) => {
                        crate::dlog!("gamepad: reading {} ({id})", path.display());
                        read(&mut file, &id, &sender);
                        crate::dlog!("gamepad: {} stopped", path.display());
                    }
                    Err(e) => crate::dlog!("gamepad: can't open {}: {e}", path.display()),
                }
            }
            sender.send_if_modified(|pad| pad.take().is_some());
            std::thread::sleep(RESCAN);
        });
        if let Err(e) = spawned {
            crate::dlog!("gamepad: no reader thread: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a resting Deck-format report, as InputPlumber's deck-uhid target writes it
    fn rest() -> [u8; REPORT_LEN] {
        let mut report = [0u8; REPORT_LEN];
        report[..4].copy_from_slice(&HEADER);
        report[4..8].copy_from_slice(&1234u32.to_le_bytes());
        report
    }

    fn put_i16(report: &mut [u8], at: usize, value: i16) {
        report[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn pressed(pad: &Pad) -> Vec<usize> {
        pad.buttons.iter().enumerate().filter(|(_, b)| b.0).map(|(i, _)| i).collect()
    }

    #[test]
    fn a_resting_report_is_centered_and_released() {
        let pad = parse_report(&rest(), "Legion Go S Controller (direct)").unwrap();
        assert_eq!(pad.axes, [0.0; 4]);
        assert_eq!(pad.buttons.len(), 17);
        assert!(pressed(&pad).is_empty());
        assert_eq!(pad.id, "Legion Go S Controller (direct)");
    }

    #[test]
    fn sticks_map_to_the_gamepad_apis_axes_with_y_down_positive() {
        let mut report = rest();
        // left stick full up and half right; right stick full left and full down
        put_i16(&mut report, 48, 16384);
        put_i16(&mut report, 50, 32767);
        put_i16(&mut report, 52, -32767);
        put_i16(&mut report, 54, -32767);
        let pad = parse_report(&report, "").unwrap();
        assert_eq!(pad.axes, [0.5, -1.0, -1.0, 1.0]);
    }

    #[test]
    fn i16_min_clamps_to_minus_one() {
        let mut report = rest();
        put_i16(&mut report, 48, i16::MIN);
        assert_eq!(parse_report(&report, "").unwrap().axes[0], -1.0);
    }

    #[test]
    fn triggers_carry_their_travel_and_bit() {
        let mut report = rest();
        report[44..46].copy_from_slice(&16384u16.to_le_bytes());
        report[46..48].copy_from_slice(&32767u16.to_le_bytes());
        report[8] = 0x01;
        let pad = parse_report(&report, "").unwrap();
        assert_eq!(pad.buttons[6], (false, 0.5));
        assert_eq!(pad.buttons[7], (true, 1.0));
    }

    #[test]
    fn each_bit_lands_on_its_standard_button() {
        // (byte, mask, standard-mapping index): Valve's bit layout (SDL controller_constants.h) to w3.org/TR/gamepad
        let cases = [
            (8, 0x80, 0),
            (8, 0x20, 1),
            (8, 0x40, 2),
            (8, 0x10, 3),
            (8, 0x08, 4),
            (8, 0x04, 5),
            (9, 0x10, 8),
            (9, 0x40, 9),
            (10, 0x40, 10),
            (11, 0x04, 11),
            (9, 0x01, 12),
            (9, 0x08, 13),
            (9, 0x04, 14),
            (9, 0x02, 15),
            (9, 0x20, 16),
        ];
        for (byte, mask, index) in cases {
            let mut report = rest();
            report[byte] = mask;
            let pad = parse_report(&report, "").unwrap();
            assert_eq!(pressed(&pad), vec![index], "byte {byte} mask {mask:#04x}");
            assert_eq!(pad.buttons[index].1, 1.0);
        }
    }

    #[test]
    fn other_reports_are_ignored() {
        let mut report = rest();
        report[2] = 0x01;
        assert_eq!(parse_report(&report, ""), None);
        assert_eq!(parse_report(&rest()[..63], ""), None);
        assert_eq!(parse_report(&[], ""), None);
    }

    #[test]
    fn deck_format_pads_are_recognized_by_their_uevent() {
        // deck3's /sys/class/hidraw/hidraw7/device/uevent (InputPlumber's pad) and hidraw6's (the keyboard interface)
        let plumber = "DRIVER=hid-generic\nHID_ID=0003:000028DE:000012FF\nHID_NAME=Legion Go S Controller\nHID_PHYS=\n";
        let keyboard = "DRIVER=hid-lenovo-go-s\nHID_ID=0003:00001A86:0000E310\nHID_NAME=wch.cn Legion Go S\n";
        assert!(is_deck_format(plumber));
        assert!(is_deck_format("HID_ID=0003:000028DE:00001205\n"));
        assert!(!is_deck_format(keyboard));
        assert!(!is_deck_format("HID_ID=0003:000028DE:00001102\n"));
        assert!(!is_deck_format(""));
    }

    #[test]
    fn the_page_gets_plain_json() {
        let pad = parse_report(&rest(), "x").unwrap();
        let json = serde_json::to_value(&pad).unwrap();
        assert_eq!(json["axes"], serde_json::json!([0.0, 0.0, 0.0, 0.0]));
        assert_eq!(json["buttons"][0], serde_json::json!([false, 0.0]));
    }
}
