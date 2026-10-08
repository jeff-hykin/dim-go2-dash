// AP mode: a Go2 can host its own Wi-Fi hotspot (the Unitree app's "AP mode": you name it, e.g. GO2-XXXXXX, and give
// it an 8+ character password), and in AP mode the dog answers at 192.168.12.1 with the same WebRTC handshake as on a
// LAN (unitree_webrtc_connect's LocalAP). This finds Go2-looking hotspots, switches this computer's Wi-Fi to one (after
// the page's confirmation), opens the drive session at 192.168.12.1, and switches back to the network it was on.
//
// Platforms: Linux / SteamOS through NetworkManager (`nmcli`, which the active desktop session may use without a
// password); macOS through `networksetup` (no scan: macOS hides Wi-Fi names from apps without Location permission, so
// there the hotspot's name is typed). The switcher is a trait so the state machine is tested without touching Wi-Fi;
// GO2_DASH_MOCK (or GO2_DASH_WIFI_MOCK=1) uses the mock one.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::api::HttpError;
use crate::app::App;

/// where a Go2 in AP mode answers
pub const AP_IP: &str = "192.168.12.1";
pub const PASSWORDS_FILE: &str = "go2_dash_hotspots.json";
/// how long to wait for the new link (an address on the hotspot, the dog's port answering)
const LINK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub ssid: String,
    /// 0..100, when the scan says
    pub signal: Option<u8>,
    pub security: String,
}

/// A Go2's hotspot name: the Unitree app suggests `GO2-XXXXXX`; people also keep `Go2_…` (its Bluetooth name) or a
/// `Unitree…` prefix. Matches a `go2` word or a `unitree` prefix, any case.
pub fn looks_like_go2(ssid: &str) -> bool {
    let lower = ssid.trim().to_lowercase();
    if lower.starts_with("unitree") {
        return true;
    }
    let bytes = lower.as_bytes();
    lower.match_indices("go2").any(|(i, _)| {
        let before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after = bytes.get(i + 3).is_none_or(|b| !b.is_ascii_digit());
        before && after
    })
}

/// `nmcli -t -f SSID,SIGNAL,SECURITY dev wifi list`: one network per line, `:` between fields, `\:` inside one.
pub fn parse_nmcli(text: &str) -> Vec<Network> {
    let mut seen: BTreeMap<String, Network> = BTreeMap::new();
    for line in text.lines() {
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        field.push(next);
                    }
                }
                ':' => fields.push(std::mem::take(&mut field)),
                _ => field.push(c),
            }
        }
        fields.push(field);
        let ssid = fields.first().cloned().unwrap_or_default();
        if ssid.is_empty() {
            continue; // a hidden network
        }
        let network = Network {
            ssid: ssid.clone(),
            signal: fields.get(1).and_then(|s| s.parse().ok()),
            security: fields.get(2).cloned().unwrap_or_default(),
        };
        // the same SSID from several access points: keep the strongest
        let keep = seen.get(&ssid).is_none_or(|old| network.signal.unwrap_or(0) > old.signal.unwrap_or(0));
        if keep {
            seen.insert(ssid, network);
        }
    }
    let mut list: Vec<Network> = seen.into_values().collect();
    list.sort_by(|a, b| b.signal.cmp(&a.signal));
    list
}

/// What changes this computer's Wi-Fi. Everything here is async and may take seconds.
#[async_trait::async_trait]
pub trait Switcher: Send + Sync {
    fn platform(&self) -> &'static str;
    /// None: this platform can't list networks (macOS without Location permission)
    async fn scan(&self) -> Result<Option<Vec<Network>>, String>;
    /// the SSID this computer is on, None when it can't tell
    async fn current(&self) -> Option<String>;
    async fn join(&self, ssid: &str, password: Option<&str>) -> Result<(), String>;
    /// rejoin a network this computer knows (saved password)
    async fn rejoin(&self, ssid: &str) -> Result<(), String>;
    /// whether this computer has an address on the dog's hotspot (192.168.12.x)
    async fn on_hotspot(&self) -> bool;
}

async fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new(cmd).args(args).output().await.map_err(|e| format!("{cmd}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { text.trim().to_string() } else { err })
    }
}

fn has_hotspot_address() -> bool {
    if_addrs::get_if_addrs()
        .map(|addrs| addrs.iter().any(|a| matches!(a.ip(), std::net::IpAddr::V4(v4) if v4.octets()[..3] == [192, 168, 12])))
        .unwrap_or(false)
}

pub struct Nmcli;

#[async_trait::async_trait]
impl Switcher for Nmcli {
    fn platform(&self) -> &'static str {
        "linux"
    }
    async fn scan(&self) -> Result<Option<Vec<Network>>, String> {
        let text = run("nmcli", &["-t", "-f", "SSID,SIGNAL,SECURITY", "dev", "wifi", "list", "--rescan", "yes"]).await?;
        Ok(Some(parse_nmcli(&text)))
    }
    async fn current(&self) -> Option<String> {
        let text = run("nmcli", &["-t", "-f", "ACTIVE,SSID", "dev", "wifi"]).await.ok()?;
        text.lines().find_map(|l| l.strip_prefix("yes:")).map(|s| s.replace("\\:", ":")).filter(|s| !s.is_empty())
    }
    async fn join(&self, ssid: &str, password: Option<&str>) -> Result<(), String> {
        let mut args = vec!["dev", "wifi", "connect", ssid];
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            args.extend(["password", password]);
        }
        run("nmcli", &args).await.map(|_| ())
    }
    async fn rejoin(&self, ssid: &str) -> Result<(), String> {
        match run("nmcli", &["connection", "up", "id", ssid]).await {
            Ok(_) => Ok(()),
            Err(_) => run("nmcli", &["dev", "wifi", "connect", ssid]).await.map(|_| ()),
        }
    }
    async fn on_hotspot(&self) -> bool {
        has_hotspot_address()
    }
}

pub struct MacOs;

#[async_trait::async_trait]
impl Switcher for MacOs {
    fn platform(&self) -> &'static str {
        "macos"
    }
    async fn scan(&self) -> Result<Option<Vec<Network>>, String> {
        Ok(None)
    }
    async fn current(&self) -> Option<String> {
        let device = mac_wifi_device().await;
        let summary = run("ipconfig", &["getsummary", &device]).await.ok()?;
        summary
            .lines()
            .find_map(|l| l.trim().strip_prefix("SSID :").map(|v| v.trim().to_string()))
            .filter(|s| !s.is_empty() && !s.to_lowercase().contains("redacted"))
    }
    async fn join(&self, ssid: &str, password: Option<&str>) -> Result<(), String> {
        let device = mac_wifi_device().await;
        let mut args = vec!["-setairportnetwork", device.as_str(), ssid];
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            args.push(password);
        }
        let out = run("networksetup", &args).await?;
        // networksetup exits 0 even when it fails, and says why
        if out.to_lowercase().contains("could not") || out.to_lowercase().contains("error") {
            return Err(out.trim().to_string());
        }
        Ok(())
    }
    async fn rejoin(&self, ssid: &str) -> Result<(), String> {
        self.join(ssid, None).await
    }
    async fn on_hotspot(&self) -> bool {
        has_hotspot_address()
    }
}

async fn mac_wifi_device() -> String {
    let ports = run("networksetup", &["-listallhardwareports"]).await.unwrap_or_default();
    let mut lines = ports.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "Hardware Port: Wi-Fi" {
            if let Some(device) = lines.next().and_then(|l| l.trim().strip_prefix("Device:")) {
                return device.trim().to_string();
            }
        }
    }
    "en0".to_string()
}

/// For tests and GO2_DASH_MOCK: a pretend Wi-Fi with two Go2 hotspots; records every switch, touches nothing.
#[derive(Default)]
pub struct MockSwitcher {
    pub current: Mutex<Option<String>>,
    pub log: Mutex<Vec<String>>,
}

impl MockSwitcher {
    pub fn on(ssid: &str) -> MockSwitcher {
        MockSwitcher { current: Mutex::new(Some(ssid.into())), log: Mutex::default() }
    }
}

#[async_trait::async_trait]
impl Switcher for MockSwitcher {
    fn platform(&self) -> &'static str {
        "mock"
    }
    async fn scan(&self) -> Result<Option<Vec<Network>>, String> {
        let fixture = "dimensional-edge:76:WPA2\nGO2-A1B2C3:64:WPA2\nGo2_MOCK1:41:WPA2\nNeighbors\\:5G:30:WPA2\n:20:WPA2\n";
        Ok(Some(parse_nmcli(fixture)))
    }
    async fn current(&self) -> Option<String> {
        self.current.lock().unwrap().clone()
    }
    async fn join(&self, ssid: &str, password: Option<&str>) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("join {ssid} {}", if password.is_some() { "(password)" } else { "(no password)" }));
        if looks_like_go2(ssid) && password.is_none_or(|p| p.len() < 8) {
            return Err("Secrets were required, but not provided (mock)".into());
        }
        *self.current.lock().unwrap() = Some(ssid.into());
        Ok(())
    }
    async fn rejoin(&self, ssid: &str) -> Result<(), String> {
        self.log.lock().unwrap().push(format!("rejoin {ssid}"));
        *self.current.lock().unwrap() = Some(ssid.into());
        Ok(())
    }
    async fn on_hotspot(&self) -> bool {
        self.current.lock().unwrap().as_deref().is_some_and(looks_like_go2)
    }
}

pub fn switcher(mock: bool) -> Arc<dyn Switcher> {
    if mock || std::env::var("GO2_DASH_WIFI_MOCK").is_ok_and(|v| v == "1") {
        Arc::new(MockSwitcher::on("Office Wi-Fi"))
    } else if cfg!(target_os = "macos") {
        Arc::new(MacOs)
    } else {
        Arc::new(Nmcli)
    }
}

/// The hotspot link: idle, or switching / linked / restoring, with the network it left.
#[derive(Clone, Debug, Default)]
pub struct Link {
    /// idle | joining | linked | restoring | error
    pub status: &'static str,
    pub ssid: Option<String>,
    pub previous: Option<String>,
    pub error: Option<String>,
}

impl Link {
    pub fn json(&self) -> Value {
        json!({ "status": if self.status.is_empty() { "idle" } else { self.status }, "ssid": self.ssid,
            "previous": self.previous, "error": self.error, "ip": AP_IP })
    }
}

impl App {
    pub fn hotspot_state(&self) -> Value {
        let mut state = self.hotspot.lock().unwrap().json();
        state["platform"] = json!(self.wifi.platform());
        state["canScan"] = json!(self.wifi.platform() != "macos");
        state["saved"] = json!(self.hotspot_passwords().keys().cloned().collect::<Vec<_>>());
        state
    }

    fn set_hotspot(&self, update: impl FnOnce(&mut Link)) {
        update(&mut self.hotspot.lock().unwrap());
        self.publish(json!({ "type": "hotspot", "hotspot": self.hotspot_state() }));
    }

    fn hotspot_passwords(&self) -> BTreeMap<String, String> {
        std::fs::read_to_string(self.data_dir().join(PASSWORDS_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Nearby Go2 hotspots (Linux: nmcli; macOS can't list Wi-Fi names, so it answers `canScan: false`).
    pub async fn hotspot_scan(&self) -> Result<Value, HttpError> {
        let scanned = self.wifi.scan().await.map_err(|e| HttpError::upstream(format!("Wi-Fi scan failed: {e}")))?;
        let current = self.wifi.current().await;
        Ok(match scanned {
            None => json!({ "canScan": false, "hotspots": [], "current": current,
                "note": "macOS doesn't let apps list Wi-Fi names: type the Go2's hotspot name (the Unitree app shows it, e.g. GO2-XXXXXX)" }),
            Some(list) => {
                let hotspots: Vec<Value> = list
                    .iter()
                    .filter(|n| looks_like_go2(&n.ssid))
                    .map(|n| json!({ "ssid": n.ssid, "signal": n.signal, "security": n.security, "current": current.as_deref() == Some(n.ssid.as_str()) }))
                    .collect();
                json!({ "canScan": true, "hotspots": hotspots, "current": current })
            }
        })
    }

    /// Switch this computer onto a Go2's hotspot, then open the drive session at 192.168.12.1. The page asks first
    /// (this computer loses its internet). A password given is saved for that hotspot (0600, this app's data dir).
    pub async fn hotspot_connect(
        self: &Arc<Self>,
        ssid: &str,
        password: Option<String>,
        robot: Option<String>,
        dry_run: bool,
    ) -> Result<Value, HttpError> {
        let ssid = ssid.trim();
        if ssid.is_empty() {
            return Err(HttpError::bad("ssid is required"));
        }
        if matches!(self.hotspot.lock().unwrap().status, "joining" | "restoring") {
            return Err(HttpError::conflict("already switching Wi-Fi"));
        }
        let saved = self.hotspot_passwords();
        let password = password.filter(|p| !p.is_empty()).or_else(|| saved.get(ssid).cloned());
        let previous = self.wifi.current().await;
        if dry_run {
            return Ok(
                json!({ "dryRun": true, "would": { "leave": previous, "join": ssid, "password": password.is_some(), "then": format!("drive {AP_IP}") } }),
            );
        }
        let leaving = self.hotspot.lock().unwrap().previous.clone().or(previous.clone());
        self.set_hotspot(|l| {
            *l = Link { status: "joining", ssid: Some(ssid.into()), previous: leaving.clone(), error: None };
        });
        if let Err(err) = self.wifi.join(ssid, password.as_deref()).await {
            let needs = err.to_lowercase().contains("secret") || err.to_lowercase().contains("password");
            let message = if needs && password.is_none() {
                format!("{ssid} needs its password (the one set in the Unitree app's AP mode)")
            } else {
                format!("couldn't join {ssid}: {err}")
            };
            self.set_hotspot(|l| {
                l.status = "error";
                l.error = Some(message.clone());
            });
            return Err(HttpError::new(if needs { 401 } else { 502 }, message));
        }
        if let Some(password) = &password {
            let mut saved = saved;
            saved.insert(ssid.into(), password.clone());
            self.save_private(PASSWORDS_FILE, json!(saved));
        }
        // wait for an address on the hotspot and the dog's port
        let deadline = std::time::Instant::now() + LINK_TIMEOUT;
        loop {
            let reachable = self.wifi.on_hotspot().await && (self.mock || self.check_ip(AP_IP).await.is_ok_and(|r| r["reachable"] == true));
            if reachable {
                break;
            }
            if std::time::Instant::now() > deadline {
                let message = format!("joined {ssid}, but no Go2 answers at {AP_IP}");
                self.set_hotspot(|l| {
                    l.status = "error";
                    l.error = Some(message.clone());
                });
                return Err(HttpError::upstream(message));
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        self.set_hotspot(|l| l.status = "linked");
        let name = robot.as_deref().and_then(|key| self.robot(key).ok()).and_then(|r| r["name"].as_str().map(str::to_string));
        let drive =
            crate::drive::Drive::open(self, robot, AP_IP.into(), name.unwrap_or_else(|| format!("Go2 via {ssid}")), None, false).await?;
        Ok(json!({ "hotspot": self.hotspot_state(), "drive": drive }))
    }

    /// Back to the network this computer was on (closing the drive session first).
    pub async fn hotspot_restore(self: &Arc<Self>) -> Result<Value, HttpError> {
        let previous = self.hotspot.lock().unwrap().previous.clone();
        if self.drive.lock().await.as_ref().is_some_and(|d| d.ip == AP_IP) {
            crate::drive::close(self).await;
        }
        let Some(previous) = previous else {
            self.set_hotspot(|l| *l = Link::default());
            return Err(HttpError::conflict(
                "this computer didn't say which Wi-Fi it was on (macOS hides it): pick your network in the Wi-Fi menu",
            ));
        };
        self.set_hotspot(|l| l.status = "restoring");
        match self.wifi.rejoin(&previous).await {
            Ok(()) => {
                self.set_hotspot(|l| *l = Link::default());
                Ok(json!({ "rejoined": previous, "hotspot": self.hotspot_state() }))
            }
            Err(err) => {
                let message = format!("couldn't rejoin {previous}: {err}");
                self.set_hotspot(|l| {
                    l.status = "error";
                    l.error = Some(message.clone());
                });
                Err(HttpError::upstream(message))
            }
        }
    }

    pub fn hotspot_forget(&self, ssid: &str) -> Value {
        let mut saved = self.hotspot_passwords();
        saved.remove(ssid);
        self.save_private(PASSWORDS_FILE, json!(saved));
        self.hotspot_state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go2_names() {
        for yes in ["GO2-A1B2C3", "Go2_18347", "go2", "Unitree_Go2_123", "unitree-dog", "My Go2", "GO2 lab"] {
            assert!(looks_like_go2(yes), "{yes}");
        }
        for no in ["dimensional-edge", "Ergo2000", "go20", "", "Cargo2x"] {
            assert!(!looks_like_go2(no), "{no}");
        }
    }

    #[test]
    fn nmcli_fixture() {
        // CudaLaptop's real output, plus an escaped colon, a hidden network and a duplicate access point
        let text = "dimensional-edge:76:WPA2\nGO2-A1B2C3:64:WPA2\nGO2-A1B2C3:80:WPA2\nCafe\\:Guest:30:\n:20:WPA2\n";
        let list = parse_nmcli(text);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0], Network { ssid: "GO2-A1B2C3".into(), signal: Some(80), security: "WPA2".into() });
        assert!(list.iter().any(|n| n.ssid == "Cafe:Guest" && n.security.is_empty()));
    }

    #[tokio::test]
    async fn connect_and_switch_back_with_the_mock() {
        let dir = std::env::temp_dir().join(format!("go2_hotspot_{}", crate::app::now_ms()));
        let app = App::new(dir.clone(), true);
        let wrong = app.hotspot_connect("GO2-A1B2C3", None, None, false).await.unwrap_err();
        assert_eq!(wrong.status, 401, "{}", wrong.message);
        let done = app.hotspot_connect("GO2-A1B2C3", Some("12345678".into()), None, false).await.unwrap();
        assert_eq!(done["hotspot"]["status"], "linked");
        assert_eq!(done["hotspot"]["previous"], "Office Wi-Fi");
        assert_eq!(done["drive"]["ip"], AP_IP);
        assert_eq!(app.hotspot_state()["saved"], json!(["GO2-A1B2C3"]));
        let back = app.hotspot_restore().await.unwrap();
        assert_eq!(back["rejoined"], "Office Wi-Fi");
        assert_eq!(app.hotspot_state()["status"], "idle");
        assert!(app.drive.lock().await.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
