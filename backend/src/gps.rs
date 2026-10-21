// A USB GPS puck (BU-353N and the like) plugged into this computer, recorded while recording: every GGA becomes a
// NavSatFix on /gps (frame "deck": the puck is on the deck, not the dog) and every checksum-valid sentence is kept
// verbatim on /gps_nmea (satellites, speed, course a fix has no field for). Ported from lite_record's sensors/gps.rs
// and nmea.rs. No puck is fine: the worker keeps looking (every USB serial port that answers in NMEA) and records nothing.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::msg::{Header, Msg};
use crate::record::{now_ns, Recorder};

pub const FRAME: &str = "deck";
/// a BU-353N's Prolific bridge runs at 4800
const BAUD: libc::speed_t = libc::B4800;
/// a puck emits once a second, so this is a couple of epochs of patience
const PROBE_WINDOW: Duration = Duration::from_millis(2500);
const SILENCE_BEFORE_GONE: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// NMEA caps a sentence at 82 characters; anything longer is line noise
const MAX_LINE: usize = 256;
/// the 1-sigma range error of a consumer receiver; HDOP scales it into a horizontal error (hence "approximated")
const USER_EQUIVALENT_RANGE_ERROR_M: f64 = 5.0;

/// The reader thread; dropping it stops it.
pub struct Gps {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub detail: Arc<Mutex<String>>,
}

impl Drop for Gps {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(recorder: Arc<Recorder>) -> Gps {
    start_on(recorder, candidate_ports)
}

/// `ports`: where to look for a receiver, each try (the tests' pty)
fn start_on(recorder: Arc<Recorder>, ports: impl Fn() -> Vec<PathBuf> + Send + 'static) -> Gps {
    let running = Arc::new(AtomicBool::new(true));
    let detail = Arc::new(Mutex::new("no gps plugged in".to_string()));
    let worker = {
        let (running, detail) = (running.clone(), detail.clone());
        std::thread::Builder::new()
            .name("gps".into())
            .spawn(move || {
                let mut last_failure = String::new();
                while running.load(Ordering::Relaxed) {
                    match open_receiver(ports(), &running) {
                        Ok((path, port)) => {
                            recorder.write("/logs", Msg::Text(json!({ "type": "gps", "state": "reading", "device": path.display().to_string() }).to_string()));
                            let reason = stream(&path, port, &running, &recorder, &detail);
                            recorder.write("/logs", Msg::Text(json!({ "type": "gps", "state": "stopped", "device": path.display().to_string(), "reason": reason }).to_string()));
                            last_failure.clear();
                        }
                        Err(failure) => {
                            // an unreadable port is worth a line once (permissions); no port at all is the normal case
                            if failure != last_failure && !failure.starts_with("no usb serial port") {
                                recorder.write("/logs", Msg::Text(json!({ "type": "gps", "state": "unavailable", "reason": failure }).to_string()));
                            }
                            *detail.lock().unwrap() = failure.clone();
                            last_failure = failure;
                        }
                    }
                    let resume_at = Instant::now() + RETRY_INTERVAL;
                    while running.load(Ordering::Relaxed) && Instant::now() < resume_at {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .ok()
    };
    Gps { running, worker, detail }
}

/// Reads until the port fails or goes quiet for good; returns why it stopped.
fn stream(path: &Path, mut port: File, running: &AtomicBool, recorder: &Recorder, detail: &Mutex<String>) -> String {
    let mut lines = LineSplitter::default();
    let mut buffer = [0u8; 512];
    let mut last_sentence = Instant::now();
    *detail.lock().unwrap() = format!("{}: waiting for a sentence", path.display());
    while running.load(Ordering::Relaxed) {
        let count = match port.read(&mut buffer) {
            Ok(count) => count,
            Err(failure) if failure.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(failure) => return format!("read failed: {failure}"),
        };
        for line in lines.push(&buffer[..count]) {
            let Some(sentence) = parse(&line) else { continue };
            last_sentence = Instant::now();
            recorder.write("/gps_nmea", Msg::Text(line));
            if let Sentence::Gga(gga) = sentence {
                *detail.lock().unwrap() = describe(path, &gga);
                recorder.write("/gps", gga.to_fix(Header { stamp_ns: now_ns(), frame_id: FRAME }));
            }
        }
        // an unplugged usb-serial bridge on Linux reads as endless zero-byte timeouts rather than an error
        if last_sentence.elapsed() >= SILENCE_BEFORE_GONE && !path.exists() {
            return "device went away".into();
        }
    }
    "recording ended".into()
}

fn describe(path: &Path, gga: &Gga) -> String {
    let fix = match (gga.has_fix(), gga.latitude, gga.longitude) {
        (true, Some(latitude), Some(longitude)) => format!("fix {latitude:.5}, {longitude:.5}"),
        _ => "no fix".into(),
    };
    format!("{} · {fix} · {} sats", path.display(), gga.satellites)
}

/// The first USB serial port that answers in NMEA.
fn open_receiver(candidates: Vec<PathBuf>, running: &AtomicBool) -> Result<(PathBuf, File), String> {
    if candidates.is_empty() {
        return Err("no usb serial port (no gps plugged in)".into());
    }
    let mut failures = Vec::new();
    for path in &candidates {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        match open_serial(path) {
            Ok(mut port) => {
                if speaks_nmea(&mut port, running) {
                    return Ok((path.clone(), port));
                }
                failures.push(format!("{}: no NMEA at 4800 baud", path.display()));
            }
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }
    Err(failures.join("; "))
}

/// Whether NMEA arrives within PROBE_WINDOW (giving up early once `running` is cleared).
fn speaks_nmea(port: &mut File, running: &AtomicBool) -> bool {
    let started = Instant::now();
    let mut lines = LineSplitter::default();
    let mut buffer = [0u8; 256];
    while started.elapsed() < PROBE_WINDOW && running.load(Ordering::Relaxed) {
        let Ok(count) = port.read(&mut buffer) else { return false };
        if lines.push(&buffer[..count]).iter().any(|line| parse(line).is_some()) {
            return true;
        }
    }
    false
}

/// USB serial ports, stable names first: Linux's /dev/serial/by-id (only USB-attached ones), then the raw node names,
/// and macOS's cu. names.
fn candidate_ports() -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |path: PathBuf| {
        if seen.insert(std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())) {
            found.push(path);
        }
    };
    sorted_entries(Path::new("/dev/serial/by-id")).into_iter().for_each(&mut add);
    for entry in sorted_entries(Path::new("/dev")) {
        let name = entry.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if ["ttyUSB", "ttyACM", "cu.usbserial", "cu.usbmodem"].iter().any(|prefix| name.starts_with(prefix)) {
            add(entry);
        }
    }
    found
}

fn sorted_entries(directory: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(directory)
        .map(|listing| listing.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

/// Raw 8N1 at 4800, reads that return after half a second of silence (so the worker notices a stop). Opened
/// non-blocking so a port with no carrier can't hang the open, then switched back to blocking reads.
fn open_serial(path: &Path) -> std::io::Result<File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path)?;
    let fd = file.as_raw_fd();
    let check = |result: libc::c_int| if result < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) };
    // SAFETY: fd is open for the lifetime of `file`, and tcgetattr fills termios in before anything reads it
    unsafe {
        let mut settings: libc::termios = std::mem::zeroed();
        check(libc::tcgetattr(fd, &mut settings))?;
        libc::cfmakeraw(&mut settings);
        settings.c_cflag |= libc::CLOCAL | libc::CREAD;
        settings.c_cc[libc::VMIN] = 0;
        settings.c_cc[libc::VTIME] = 5;
        check(libc::cfsetispeed(&mut settings, BAUD))?;
        check(libc::cfsetospeed(&mut settings, BAUD))?;
        check(libc::tcsetattr(fd, libc::TCSANOW, &settings))?;
        check(libc::tcflush(fd, libc::TCIFLUSH))?;
        let flags = libc::fcntl(fd, libc::F_GETFL);
        check(flags)?;
        check(libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK))?;
    }
    Ok(file)
}

/// Splits a byte stream into lines, tolerating the garbage a mid-sentence open or a wrong baud rate produces.
#[derive(Default)]
struct LineSplitter {
    partial: Vec<u8>,
    /// the current line outgrew MAX_LINE: drop it through its line ending
    overflowed: bool,
}

impl LineSplitter {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\n' || byte == b'\r' {
                if !self.partial.is_empty() && !self.overflowed {
                    lines.push(String::from_utf8_lossy(&self.partial).into_owned());
                }
                self.partial.clear();
                self.overflowed = false;
            } else if self.partial.len() < MAX_LINE {
                self.partial.push(byte);
            } else {
                self.partial.clear();
                self.overflowed = true;
            }
        }
        lines
    }
}

/// A GGA sentence: position, altitude, satellites and HDOP, once per epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct Gga {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// 0 invalid, 1 GPS, 2 DGPS/SBAS, 4 RTK fixed, 5 RTK float, 6 dead reckoning
    pub quality: u8,
    pub satellites: u8,
    pub hdop: Option<f64>,
    pub altitude_msl: Option<f64>,
    /// the geoid's height above the WGS84 ellipsoid
    pub geoid_separation: Option<f64>,
}

impl Gga {
    pub fn has_fix(&self) -> bool {
        self.quality != 0 && self.latitude.is_some() && self.longitude.is_some()
    }

    /// NavSatFix (sensor_msgs/NavSatStatus values): altitude above the WGS84 ellipsoid (GGA gives it above the geoid),
    /// NaN position without a fix, an HDOP-derived covariance with one.
    pub fn to_fix(&self, header: Header<'static>) -> Msg {
        let status = match (self.has_fix(), self.quality) {
            (false, _) => -1,
            (true, 2) => 1,
            (true, 4 | 5) => 2,
            _ => 0,
        };
        let mut covariance = [0.0; 9];
        let covariance_type = match self.hdop {
            Some(hdop) if self.has_fix() => {
                let horizontal = (hdop * USER_EQUIVALENT_RANGE_ERROR_M).powi(2);
                covariance[0] = horizontal;
                covariance[4] = horizontal;
                // vertical error runs about twice the horizontal
                covariance[8] = 4.0 * horizontal;
                1
            }
            _ => 0,
        };
        let altitude = self.altitude_msl.map(|msl| msl + self.geoid_separation.unwrap_or(0.0));
        Msg::NavSatFix {
            header,
            status,
            latitude: self.latitude.unwrap_or(f64::NAN),
            longitude: self.longitude.unwrap_or(f64::NAN),
            altitude: altitude.unwrap_or(f64::NAN),
            covariance,
            covariance_type,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sentence {
    Gga(Gga),
    /// checksum-valid, but not a sentence this reads
    Other,
}

/// One line; None when it isn't a checksum-valid NMEA sentence (which is also how a wrong baud rate looks).
pub fn parse(line: &str) -> Option<Sentence> {
    let rest = line.trim().strip_prefix('$')?;
    let (body, checksum) = rest.rsplit_once('*')?;
    let expected = u8::from_str_radix(checksum.get(..2)?, 16).ok()?;
    if body.bytes().fold(0u8, |sum, byte| sum ^ byte) != expected {
        return None;
    }
    let fields: Vec<&str> = body.split(',').collect();
    // the first two letters are the talker (GP, GN, GL, …), the rest the type
    let kind = fields[0];
    if kind.len() != 5 || &kind[2..] != "GGA" {
        return Some(Sentence::Other);
    }
    if fields.len() < 12 {
        return None;
    }
    Some(Sentence::Gga(Gga {
        latitude: coordinate(fields[2], fields[3], 2),
        longitude: coordinate(fields[4], fields[5], 3),
        quality: fields[6].parse().unwrap_or(0),
        satellites: fields[7].parse().unwrap_or(0),
        hdop: fields[8].parse().ok(),
        altitude_msl: fields[9].parse().ok(),
        geoid_separation: fields[11].parse().ok(),
    }))
}

/// ddmm.mmmm (dddmm.mmmm for longitude) and a hemisphere letter, to signed decimal degrees
fn coordinate(value: &str, hemisphere: &str, degree_digits: usize) -> Option<f64> {
    if value.len() <= degree_digits {
        return None;
    }
    let magnitude = value[..degree_digits].parse::<f64>().ok()? + value[degree_digits..].parse::<f64>().ok()? / 60.0;
    match hemisphere {
        "N" | "E" => Some(magnitude),
        "S" | "W" => Some(-magnitude),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // captured from the BU-353N on dimpi5, 2026-09-26, indoors in San Francisco (lite_record's nmea.rs)
    const GGA: &str = "$GPGGA,175411.000,3745.7561,N,12229.6726,W,1,8,1.30,55.8,M,-25.4,M,,0000*5A";
    const RMC: &str = "$GPRMC,175411.000,A,3745.7561,N,12229.6726,W,0.09,179.20,260926,,,A*79";

    fn with_checksum(body: &str) -> String {
        format!("${body}*{:02X}", body.bytes().fold(0u8, |sum, byte| sum ^ byte))
    }

    #[test]
    fn a_real_gga_parses_to_san_francisco_with_an_ellipsoid_altitude() {
        let Some(Sentence::Gga(gga)) = parse(GGA) else { panic!("not a gga") };
        assert!(gga.has_fix());
        assert!((gga.latitude.unwrap() - 37.762_601_7).abs() < 1e-6);
        assert!((gga.longitude.unwrap() + 122.494_543_3).abs() < 1e-6);
        assert_eq!((gga.satellites, gga.hdop), (8, Some(1.30)));
        let Msg::NavSatFix { header, status, altitude, covariance, covariance_type, .. } = gga.to_fix(Header { stamp_ns: 5, frame_id: FRAME }) else {
            panic!("not a fix")
        };
        assert_eq!((header.frame_id, status, covariance_type), ("deck", 0, 1));
        assert!((altitude - 30.4).abs() < 1e-9);
        assert!((covariance[8] - 4.0 * (1.3f64 * 5.0).powi(2)).abs() < 1e-9);
    }

    #[test]
    fn other_sentences_are_kept_and_corrupt_ones_rejected() {
        assert_eq!(parse(RMC), Some(Sentence::Other));
        assert_eq!(parse(&GGA.replace("3745", "3746")), None);
        assert_eq!(parse("not nmea at all"), None);
        assert_eq!(parse("$GPGGA,no,checksum"), None);
    }

    #[test]
    fn no_fix_is_nan_with_an_unknown_covariance() {
        let Some(Sentence::Gga(gga)) = parse(&with_checksum("GPGGA,175411.000,,,,,0,0,,,M,,M,,")) else { panic!("not a gga") };
        let Msg::NavSatFix { status, latitude, covariance_type, .. } = gga.to_fix(Header { stamp_ns: 1, frame_id: FRAME }) else {
            panic!("not a fix")
        };
        assert_eq!((status, covariance_type), (-1, 0));
        assert!(latitude.is_nan());
    }

    #[test]
    fn a_pseudo_terminal_speaking_nmea_is_recorded_as_deck_fixes() {
        // a pty stands in for the serial bridge: the same termios calls and read path, no hardware
        use std::io::Write;
        use std::os::fd::FromRawFd;
        // SAFETY: the standard posix_openpt sequence; the fd is owned by the File
        let (controller, device) = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0 && libc::grantpt(fd) == 0 && libc::unlockpt(fd) == 0);
            let name = std::ffi::CStr::from_ptr(libc::ptsname(fd)).to_string_lossy().into_owned();
            (File::from_raw_fd(fd), PathBuf::from(name))
        };
        let dir = std::env::temp_dir().join(format!("go2_gps_test_{}", now_ns()));
        let path = dir.join("a.db");
        let recorder = Arc::new(Recorder::start(&path, Default::default(), |_| Default::default()).unwrap());
        let gps = start_on(recorder.clone(), move || vec![device.clone()]);
        let mut writer = &controller;
        let deadline = Instant::now() + Duration::from_secs(8);
        // the probe reads up to 2.5 s: keep talking, once a second like a puck, until it's past the probe and recording
        while !gps.detail.lock().unwrap().contains("8 sats") && Instant::now() < deadline {
            writer.write_all(format!("garbage\r\n{GGA}\r\n{RMC}\r\n").as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        }
        drop(gps);
        recorder.finish().unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let fix: Vec<u8> = connection.query_row("SELECT data FROM gps JOIN gps_blob USING (id) LIMIT 1", [], |r| r.get(0)).unwrap();
        let fix = lcm_msgs::sensor_msgs::NavSatFix::decode(&fix).unwrap();
        assert_eq!((fix.header.frame_id.as_str(), fix.status.status, fix.status.service), ("deck", 0, 1));
        assert!((fix.latitude - 37.7626).abs() < 1e-3 && (fix.longitude + 122.4945).abs() < 1e-3);
        let sentences: i64 = connection.query_row("SELECT count(*) FROM gps_nmea", [], |r| r.get(0)).unwrap();
        assert!(sentences >= 2, "GGA and RMC kept verbatim, garbage not");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lines_split_across_reads_are_reassembled_and_noise_dropped() {
        let mut lines = LineSplitter::default();
        assert!(lines.push(b"$GPGGA,1").is_empty());
        assert_eq!(lines.push(b"23*00\r\n$GPR"), vec!["$GPGGA,123*00".to_string()]);
        assert!(lines.push(&[b'x'; 10_000]).is_empty());
        assert_eq!(lines.push(b"\n$ok*00\n"), vec!["$ok*00".to_string()]);
    }
}
