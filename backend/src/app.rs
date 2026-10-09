// Everything the app knows and does, behind the routes: discovered robots (BLE + LAN + ARP scans), the names / AES keys
// / last-known IPs the user gave them (persisted), Wi-Fi provisioning over Bluetooth, Unitree cloud accounts (for AES
// keys), this computer's network, and the live drive session (drive.rs). Every change is published as an event (to the
// page's zenoh topic `events`, through Desktop's relay: relay.rs) so open pages follow what the agent does and vice versa.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::platform::Adapter;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use crate::api::{HttpError, Route};
use crate::ble::{self, Registry};
use crate::discovery::{self, Sink};
use crate::drive::Drive;

const NAMES_FILE: &str = "go2_dash_names.json";
const AES_KEYS_FILE: &str = "go2_dash_aes_keys.json";
const ACCOUNTS_FILE: &str = "go2_dash_unitree_accounts.json";
const IPS_FILE: &str = "go2_dash_ips.json";
/// where Unitree MACs were seen on the network, newest first (the sweep re-pings these first)
const SEEN_FILE: &str = "go2_dash_seen.json";
const SEEN_MAX: usize = 64;
const WIDEN_EVERY: Duration = Duration::from_secs(600);
pub const MULTICAST_GROUP: &str = "231.1.1.1";

#[derive(Clone)]
enum AdapterState {
    Pending,
    Ready(Adapter),
    Failed(String),
}

#[derive(Clone, Default)]
struct Account {
    password: String,
    last_pull: Option<u64>,
    robots: Vec<Value>,
    error: Option<String>,
    pulling: bool,
}

#[derive(Default)]
struct State {
    /// raw discovery records, keyed by serial || ble_mac || ip (the key the UI and agent use)
    devices: BTreeMap<String, Value>,
    names: BTreeMap<String, String>,
    aes_keys: BTreeMap<String, String>,
    ips: BTreeMap<String, String>,
    accounts: BTreeMap<String, Account>,
    scanning: bool,
    last_count: Option<usize>,
    notice: Option<String>,
    /// the ARP sweep: what it covers and how far it got (discovery.rs `sweep` events), null before one
    sweep: Value,
    /// [{ip, mac, at}] (SEEN_FILE)
    seen: Vec<Value>,
    stop: Option<Arc<discovery::Stop>>,
    /// when a quick scan last widened its sweep to the whole subnet (at most once per WIDEN_EVERY)
    last_widen: Option<std::time::Instant>,
    wifi: Value,
    host_ssid: String,
    host_ssid_status: &'static str,
}

pub struct App {
    pub routes: Vec<Route>,
    /// GO2_DASH_MOCK=1 (and the tests): scans, Wi-Fi provisioning, drive sessions and cloud pulls are simulated —
    /// nothing touches Bluetooth, the network or a robot
    pub mock: bool,
    data_dir: PathBuf,
    events: broadcast::Sender<String>,
    state: Mutex<State>,
    registry: Registry,
    adapter: tokio::sync::OnceCell<watch::Receiver<AdapterState>>,
    scan_done: watch::Sender<u64>,
    wifi_task: tokio::sync::Mutex<Option<tokio::task::AbortHandle>>,
    pub drive: tokio::sync::Mutex<Option<Arc<Drive>>>,
    /// the first-run guide's step and robot (setup.rs)
    pub(crate) setup: Mutex<Value>,
    pub(crate) mock_launch: crate::setup::MockLaunch,
    /// Desktop's URL (DIMOS_APP's desktopUrl): where dimos is launched
    pub desktop_url: std::sync::OnceLock<String>,
    /// mock only: when each robot given Wi-Fi shows up on the network (ms), as a real one does ~30 s later
    mock_joined: Mutex<BTreeMap<String, u64>>,
    /// the recording in progress (recording.rs)
    pub(crate) recording: Mutex<Option<Arc<crate::recording::Active>>>,
    /// Desktop's recordings folder (DIMOS_APP's recordingsDir); this app records into its `go2/`
    pub recordings_root: Option<PathBuf>,
    /// per recording: robot, times, upload (uploads.rs)
    pub(crate) recordings_index: Mutex<serde_json::Map<String, Value>>,
    /// {autoUpload}
    pub(crate) settings: Mutex<Value>,
    pub(crate) upload_watcher: std::sync::atomic::AtomicBool,
    /// this computer's Wi-Fi (hotspot.rs: AP mode)
    pub(crate) wifi: Arc<dyn crate::hotspot::Switcher>,
    pub(crate) hotspot: Mutex<crate::hotspot::Link>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn device_key(device: &Value) -> String {
    for field in ["serial", "ble_mac", "ip"] {
        if let Some(value) = device[field].as_str().filter(|v| !v.is_empty()) {
            return value.to_string();
        }
    }
    device.to_string()
}

const SAVED_FILES: [&str; 5] = [NAMES_FILE, AES_KEYS_FILE, ACCOUNTS_FILE, IPS_FILE, crate::setup::SETUP_FILE];

/// Where the app keeps what it saves: GO2_DASH_DATA_DIR, else the data folder Desktop gives it (DIMOS_APP's dataDir:
/// each Desktop, and each DIMOS_HOME, its own), else ~/.local/share/dim. Mock mode keeps its own `mock/` beside it, so
/// a simulation never writes into a real robot list.
pub fn data_dir(explicit: Option<PathBuf>, desktop: Option<PathBuf>, home: PathBuf, mock: bool) -> PathBuf {
    let dir = explicit.or(desktop).unwrap_or_else(|| home.join(".local/share/dim"));
    if mock {
        dir.join("mock")
    } else {
        dir
    }
}

/// Before Desktop gave apps a data folder everything lived in ~/.local/share/dim: copy it over once, so an update
/// keeps someone's robot names, IPs, AES keys and accounts. Nothing happens once `to` has any of them.
pub fn migrate_legacy(from: &std::path::Path, to: &std::path::Path) -> usize {
    if from == to || SAVED_FILES.iter().any(|file| to.join(file).exists()) {
        return 0;
    }
    let _ = std::fs::create_dir_all(to);
    SAVED_FILES.iter().filter(|file| std::fs::copy(from.join(file), to.join(file)).is_ok()).count()
}

pub fn valid_ipv4(text: &str) -> bool {
    text.parse::<std::net::Ipv4Addr>().is_ok()
}

fn load<T: serde::de::DeserializeOwned + Default>(dir: &std::path::Path, file: &str) -> T {
    std::fs::read_to_string(dir.join(file)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

impl App {
    pub fn new(data_dir: PathBuf, mock: bool) -> Arc<App> {
        Self::with_recordings(data_dir, mock, None)
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn with_recordings(data_dir: PathBuf, mock: bool, recordings_root: Option<PathBuf>) -> Arc<App> {
        let mut state = State { host_ssid_status: "unknown", wifi: json!({ "status": "idle" }), ..Default::default() };
        state.names = load(&data_dir, NAMES_FILE);
        state.aes_keys = load(&data_dir, AES_KEYS_FILE);
        state.ips = load(&data_dir, IPS_FILE);
        state.seen = load(&data_dir, SEEN_FILE);
        let accounts: BTreeMap<String, Value> = load(&data_dir, ACCOUNTS_FILE);
        for (email, account) in accounts {
            state.accounts.insert(
                email,
                Account {
                    password: account["password"].as_str().unwrap_or("").to_string(),
                    last_pull: account["lastPull"].as_u64(),
                    robots: account["robots"].as_array().cloned().unwrap_or_default(),
                    error: account["error"].as_str().map(str::to_string),
                    pulling: false,
                },
            );
        }
        // a first run: nothing saved yet (no names, IPs, keys or accounts)
        let first_run = state.names.is_empty() && state.ips.is_empty() && state.aes_keys.is_empty() && state.accounts.is_empty();
        let setup = std::fs::read_to_string(data_dir.join(crate::setup::SETUP_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(|setup| setup["step"].is_string())
            .unwrap_or_else(|| crate::setup::initial(first_run));
        let (events, _) = broadcast::channel(256);
        let recordings_index: serde_json::Map<String, Value> = load(&data_dir, crate::uploads::INDEX_FILE);
        let mut settings: Value = load::<Option<Value>>(&data_dir, crate::uploads::SETTINGS_FILE).unwrap_or(json!({}));
        if !settings["autoUpload"].is_boolean() {
            settings["autoUpload"] = json!(true);
        }
        if !settings["recordOptions"].is_object() {
            settings["recordOptions"] = serde_json::to_value(crate::record::RecordOptions::default()).unwrap();
        }
        Arc::new(App {
            routes: crate::routes::routes(),
            mock,
            data_dir,
            events,
            state: Mutex::new(state),
            registry: Default::default(),
            adapter: tokio::sync::OnceCell::new(),
            scan_done: watch::channel(0).0,
            wifi_task: tokio::sync::Mutex::new(None),
            drive: tokio::sync::Mutex::new(None),
            setup: Mutex::new(setup),
            mock_launch: Default::default(),
            desktop_url: Default::default(),
            mock_joined: Default::default(),
            recording: Mutex::new(None),
            recordings_root,
            recordings_index: Mutex::new(recordings_index),
            settings: Mutex::new(settings),
            upload_watcher: Default::default(),
            wifi: crate::hotspot::switcher(mock),
            hotspot: Mutex::new(Default::default()),
        })
    }

    // ── events ──

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    pub fn publish(&self, event: Value) {
        if let Some(active) = self.recording() {
            if active.recorder.logs_on() {
                active.recorder.write("/logs", crate::cdr::string(&event.to_string()), None);
            }
        }
        let _ = self.events.send(event.to_string());
    }

    fn publish_robots(&self) {
        let robots = self.robots();
        self.publish(json!({ "type": "robots", "robots": robots }));
    }

    pub(crate) fn save_private(&self, file: &str, data: Value) {
        self.save(file, data, true)
    }

    pub(crate) fn save_file(&self, file: &str, data: Value) {
        self.save(file, data, false)
    }

    fn save(&self, file: &str, data: Value, private: bool) {
        let path = self.data_dir.join(file);
        let _ = std::fs::create_dir_all(&self.data_dir);
        if let Err(err) = std::fs::write(&path, serde_json::to_string_pretty(&data).unwrap_or_default()) {
            eprintln!("go2_dash: could not save {} — {err}", path.display());
        }
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }

    // ── robots ──

    fn robot_view(state: &State, key: &str, raw: &Value) -> Value {
        let scanned_ip = raw["ip"].as_str().filter(|ip| !ip.is_empty());
        let remembered = state.ips.get(key).map(String::as_str);
        let ip = scanned_ip.or(remembered);
        let custom = state.names.get(key).cloned();
        let serial = raw["serial"].as_str();
        let ble_id = raw["ble_mac"].as_str();
        let arp_only = raw["arp_only"].as_bool().unwrap_or(false);
        let display = custom.clone().or_else(|| raw["name"].as_str().map(str::to_string)).unwrap_or_else(|| match (serial, ble_id) {
            (Some(serial), _) => format!("Go2 {}", &serial[serial.len().saturating_sub(5)..]),
            (_, Some(ble)) => ble.to_string(),
            _ if arp_only => "Possible Go2".to_string(),
            _ => "Unknown Go2".to_string(),
        });
        json!({
            "key": key,
            "name": display,
            "customName": custom,
            "bleName": raw["name"],
            "serial": serial,
            "bleId": ble_id,
            "ip": ip,
            "ipSource": if scanned_ip.is_some() { json!("scan") } else if remembered.is_some() { json!("remembered") } else { Value::Null },
            "lanMac": raw["lan_mac"],
            "bleMac": raw["ble_hw_mac"],
            "matched": raw["matched"].as_str().unwrap_or(if arp_only { "oui" } else if scanned_ip.is_some() { "lan" } else { "ble" }),
            "arpOnly": arp_only,
            "hasAesKey": state.aes_keys.contains_key(key),
            "canProvisionWifi": ble_id.is_some() && !arp_only,
        })
    }

    /// The scanned robots, with the user's names, remembered IPs and AES-key status folded in.
    pub fn robots(&self) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        state
            .devices
            .iter()
            // an ARP-only "possible Go2" at a known dog's remembered IP is that dog
            .filter(|(key, raw)| !(raw["arp_only"] == true && state.ips.iter().any(|(other, ip)| other != *key && ip == *key)))
            .map(|(key, raw)| Self::robot_view(&state, key, raw))
            .collect()
    }

    pub fn robot(&self, key: &str) -> Result<Value, HttpError> {
        let state = self.state.lock().unwrap();
        let raw = state.devices.get(key).ok_or_else(|| {
            HttpError::not_found(format!("no robot {key} — scan first (POST api/scan), then use a key from GET api/robots"))
        })?;
        Ok(Self::robot_view(&state, key, raw))
    }

    pub fn rename(&self, key: &str, name: &str) -> Result<Value, HttpError> {
        self.robot(key)?;
        let names = {
            let mut state = self.state.lock().unwrap();
            let name = name.trim();
            if name.is_empty() {
                state.names.remove(key);
            } else {
                state.names.insert(key.to_string(), name.to_string());
            }
            json!(state.names)
        };
        self.save(NAMES_FILE, names, false);
        self.publish_robots();
        self.robot(key)
    }

    pub fn set_aes_key(&self, key: &str, aes_key: &str) -> Result<Value, HttpError> {
        let aes_key = aes_key.trim().to_lowercase();
        if !aes_key.is_empty() {
            crate::robot_rtc::parse_aes_key(&aes_key).map_err(HttpError::bad)?;
        }
        self.robot(key)?;
        let keys = {
            let mut state = self.state.lock().unwrap();
            if aes_key.is_empty() {
                state.aes_keys.remove(key);
            } else {
                state.aes_keys.insert(key.to_string(), aes_key);
            }
            json!(state.aes_keys)
        };
        self.save(AES_KEYS_FILE, keys, true);
        self.publish_robots();
        self.robot(key)
    }

    pub fn set_ip(&self, key: &str, ip: &str) -> Result<Value, HttpError> {
        let ip = ip.trim();
        if !ip.is_empty() && !valid_ipv4(ip) {
            return Err(HttpError::bad(format!("{ip} isn't an IPv4 address")));
        }
        self.robot(key)?;
        self.remember_ip(key, ip);
        self.publish_robots();
        self.robot(key)
    }

    pub(crate) fn remember_ip_public(&self, key: &str, ip: &str) {
        self.remember_ip(key, ip);
        self.publish_robots();
    }

    fn remember_ip(&self, key: &str, ip: &str) {
        let ips = {
            let mut state = self.state.lock().unwrap();
            if state.ips.get(key).map(String::as_str) == Some(ip) || (ip.is_empty() && !state.ips.contains_key(key)) {
                return;
            }
            if ip.is_empty() {
                state.ips.remove(key);
            } else {
                state.ips.insert(key.to_string(), ip.to_string());
            }
            json!(state.ips)
        };
        self.save(IPS_FILE, ips, false);
    }

    pub fn aes_key_for(&self, key: &str) -> String {
        let state = self.state.lock().unwrap();
        if let Some(aes) = state.aes_keys.get(key) {
            return aes.clone();
        }
        if let Some(aes) = state
            .names
            .iter()
            .find_map(|(robot, name)| (key == name || key.starts_with(&format!("{name}_"))).then(|| state.aes_keys.get(robot)).flatten())
        {
            return aes.clone();
        }
        let serial = state.devices.get(key).and_then(|d| d["serial"].as_str()).unwrap_or(key).to_string();
        if let Some(aes) = state.aes_keys.get(&serial) {
            return aes.clone();
        }
        drop(state);
        shared_aes_key(&serial).unwrap_or_default()
    }

    /// Every AES key this computer knows, labelled by where it came from: the ones saved per robot, then dimos'
    /// fleet key file (~/.config/dimos/go2-keys, "SERIAL KEY ALIAS" per line, the SteamOS installer writes it).
    pub fn known_aes_keys(&self) -> Vec<crate::robot_rtc::AesKey> {
        let mut keys: Vec<crate::robot_rtc::AesKey> =
            self.state.lock().unwrap().aes_keys.iter().map(|(robot, key)| (format!("saved for {robot}"), key.clone())).collect();
        if self.mock {
            return keys;
        }
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let text = std::fs::read_to_string(home.join(".config/dimos/go2-keys")).unwrap_or_default();
        keys.extend(fleet_keys(&text));
        keys
    }

    // ── scanning ──

    pub fn scan_state(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({ "scanning": state.scanning, "lastCount": state.last_count, "notice": state.notice, "sweep": state.sweep })
    }

    fn on_discovery(self: &Arc<Self>, event: Value) {
        match event["type"].as_str().unwrap_or("") {
            "device" => {
                let key = device_key(&event);
                let ip = event["ip"].as_str().map(str::to_string);
                {
                    let mut state = self.state.lock().unwrap();
                    let mut record = event.clone();
                    record.as_object_mut().unwrap().remove("type");
                    // a serial-bearing record supersedes the serial-less card for the same BLE id
                    if let Some(ble) = record["ble_mac"].as_str().filter(|_| record["serial"].is_string()) {
                        let stale: Vec<String> = state
                            .devices
                            .iter()
                            .filter(|(_, d)| !d["serial"].is_string() && d["ble_mac"] == ble)
                            .map(|(k, _)| k.clone())
                            .collect();
                        for stale_key in stale {
                            state.devices.remove(&stale_key);
                        }
                    }
                    state.devices.insert(key.clone(), record);
                }
                if let Some(ip) = ip.filter(|_| event["arp_only"] != true) {
                    self.remember_ip(&key, &ip);
                }
                self.publish_robots();
            }
            "sweep" => {
                let mut sweep = event.clone();
                sweep.as_object_mut().unwrap().remove("type");
                let mut state = self.state.lock().unwrap();
                if sweep["phase"] == "widen" {
                    state.last_widen = Some(std::time::Instant::now());
                }
                state.sweep = sweep;
                drop(state);
                self.publish(json!({ "type": "scan", "scan": self.scan_state() }));
            }
            "seen" => {
                let (Some(ip), Some(mac)) = (event["ip"].as_str(), event["mac"].as_str()) else { return };
                let seen = {
                    let mut state = self.state.lock().unwrap();
                    state.seen.retain(|row| row["mac"] != mac && row["ip"] != ip);
                    state.seen.insert(0, json!({ "ip": ip, "mac": mac, "at": now_ms() }));
                    state.seen.truncate(SEEN_MAX);
                    state.seen.clone()
                };
                self.save_file(SEEN_FILE, json!(seen));
            }
            "drop" => {
                if let Some(key) = event["key"].as_str() {
                    self.state.lock().unwrap().devices.remove(key);
                    self.publish_robots();
                }
            }
            "warn" => {
                let message = event["msg"].as_str().unwrap_or("").to_string();
                eprintln!("go2_dash: {message}");
                if event["kind"] == "bluetooth_pending" || message.starts_with("bluetooth:") || message.starts_with("ble:") {
                    self.state.lock().unwrap().notice = Some(message);
                    self.publish(json!({ "type": "scan", "scan": self.scan_state() }));
                }
            }
            _ => {}
        }
    }

    /// The BLE adapter, probed once in the background. On macOS the first CoreBluetooth call blocks until the user
    /// answers the Bluetooth permission prompt, so a scan waits only briefly and otherwise goes ahead over LAN/ARP.
    async fn adapter_for_scan(self: &Arc<Self>) -> Option<Adapter> {
        let mut rx = self
            .adapter
            .get_or_init(|| async {
                let (tx, rx) = watch::channel(AdapterState::Pending);
                tokio::spawn(async move {
                    let _ = tx.send(match ble::first_adapter().await {
                        Ok(adapter) => AdapterState::Ready(adapter),
                        Err(err) => AdapterState::Failed(err),
                    });
                });
                rx
            })
            .await
            .clone();
        if matches!(*rx.borrow(), AdapterState::Pending) {
            let _ = tokio::time::timeout(Duration::from_millis(1500), rx.changed()).await;
        }
        let adapter_state = rx.borrow().clone();
        match adapter_state {
            AdapterState::Ready(adapter) => Some(adapter),
            AdapterState::Failed(err) => {
                self.on_discovery(json!({ "type": "warn", "msg": format!("bluetooth: {err} — scanning the network only.") }));
                None
            }
            AdapterState::Pending => {
                self.on_discovery(json!({ "type": "warn", "kind": "bluetooth_pending", "msg": "Bluetooth permission pending — click Allow on this computer's Bluetooth prompt, then scan again. Scanning the network only." }));
                None
            }
        }
    }

    /// Starts a scan (a re-scan forgets the previous results) unless one is running, and, with `wait`, returns the
    /// robots once it ends.
    pub async fn scan(self: &Arc<Self>, timeout_secs: f64, wait: bool, sweep: &str) -> Result<Value, HttpError> {
        if !(1.0..=60.0).contains(&timeout_secs) {
            return Err(HttpError::bad("timeout must be between 1 and 60 seconds"));
        }
        let sweep = discovery::SweepMode::parse(sweep).ok_or_else(|| HttpError::bad("sweep must be quick, full, known or off"))?;
        let stop = Arc::new(discovery::Stop::default());
        let mut done = self.scan_done.subscribe();
        let start = {
            let mut state = self.state.lock().unwrap();
            if state.scanning {
                false
            } else {
                state.scanning = true;
                state.devices.clear();
                state.notice = None;
                state.sweep = Value::Null;
                state.stop = Some(stop.clone());
                true
            }
        };
        if start {
            self.publish(json!({ "type": "scan", "scan": self.scan_state() }));
            self.publish_robots();
            let app = self.clone();
            tokio::spawn(async move {
                app.refresh_ssid().await;
                let sink: Sink = {
                    let app = app.clone();
                    Arc::new(move |event| app.on_discovery(event))
                };
                if app.mock {
                    let joined: Vec<String> =
                        app.mock_joined.lock().unwrap().iter().filter(|(_, at)| **at <= now_ms()).map(|(key, _)| key.clone()).collect();
                    mock_scan(sink, timeout_secs, &joined, sweep, &stop).await;
                } else {
                    let adapter = app.adapter_for_scan().await;
                    let (known, widen) = {
                        let state = app.state.lock().unwrap();
                        let known = state.seen.iter().filter_map(|row| row["ip"].as_str()?.parse().ok()).collect();
                        (known, state.last_widen.is_none_or(|at| at.elapsed() > WIDEN_EVERY))
                    };
                    let options = discovery::ScanOptions { timeout_secs, sweep, known, widen, stop };
                    discovery::do_scan(adapter, app.registry.clone(), options, sink).await;
                }
                let count = {
                    let mut state = app.state.lock().unwrap();
                    state.scanning = false;
                    state.stop = None;
                    state.last_count = Some(state.devices.len());
                    state.devices.len()
                };
                app.publish(json!({ "type": "scan", "scan": app.scan_state() }));
                app.scan_done.send_modify(|n| *n += 1);
                eprintln!("go2_dash: scan done, {count} found");
            });
        }
        if wait {
            let _ = done.changed().await;
        }
        Ok(json!({ "scan": self.scan_state(), "robots": self.robots() }))
    }

    /// Ends the running scan now (the sweep stops at its next batch); what it found so far stays.
    pub fn stop_scan(&self) -> Value {
        let stop = self.state.lock().unwrap().stop.clone();
        if let Some(stop) = &stop {
            stop.stop();
        }
        json!({ "stopped": stop.is_some(), "scan": self.scan_state() })
    }

    // ── this computer's network ──

    pub async fn refresh_ssid(&self) {
        let raw = if self.mock { "MockNet".to_string() } else { detect_ssid().await };
        {
            let mut state = self.state.lock().unwrap();
            if raw.is_empty() {
                state.host_ssid = String::new();
                state.host_ssid_status = "unknown";
            } else if raw.to_lowercase().contains("redacted") {
                // macOS hides the SSID without Location Services: say so instead of guessing
                state.host_ssid = String::new();
                state.host_ssid_status = "redacted";
            } else {
                state.host_ssid = raw;
                state.host_ssid_status = "ok";
            }
        }
        self.publish(json!({ "type": "network", "network": self.network_basic() }));
    }

    pub fn network_basic(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({ "ssid": if state.host_ssid.is_empty() { Value::Null } else { json!(state.host_ssid) }, "ssidStatus": state.host_ssid_status, "mock": self.mock })
    }

    /// SSID plus whether LAN discovery's multicast probe can leave on the Wi-Fi interface (a VPN can hijack it).
    pub async fn network(&self) -> Value {
        self.refresh_ssid().await;
        let mut network = self.network_basic();
        if cfg!(target_os = "macos") && !self.mock {
            let wifi = wifi_device().await;
            let routed = route_interface(MULTICAST_GROUP).await;
            network["multicast"] = json!({
                "group": MULTICAST_GROUP,
                "routedVia": routed,
                "wifiInterface": wifi,
                "ok": routed.as_deref() == Some(wifi.as_str()),
                "fix": format!("sudo route -n add -host {MULTICAST_GROUP} -interface {wifi}"),
            });
        }
        network
    }

    pub fn host_ssid(&self) -> Option<String> {
        let state = self.state.lock().unwrap();
        (state.host_ssid_status == "ok").then(|| state.host_ssid.clone())
    }

    // ── Wi-Fi provisioning over Bluetooth ──

    pub fn wifi_state(&self) -> Value {
        self.state.lock().unwrap().wifi.clone()
    }

    fn set_wifi(&self, update: impl FnOnce(&mut Value)) {
        let wifi = {
            let mut state = self.state.lock().unwrap();
            update(&mut state.wifi);
            state.wifi.clone()
        };
        self.publish(json!({ "type": "wifi", "wifi": wifi }));
    }

    fn wifi_log(&self, line: String) {
        self.set_wifi(|wifi| {
            if let Some(log) = wifi["log"].as_array_mut() {
                log.push(json!(line));
            }
        });
    }

    /// Sends Wi-Fi credentials to a robot over Bluetooth so it joins that network; returns once it accepted them (or
    /// failed). `dry_run` checks everything and says what it would send, without touching Bluetooth.
    pub async fn provision_wifi(
        self: &Arc<Self>,
        key: &str,
        ssid: &str,
        password: &str,
        country: &str,
        dry_run: bool,
    ) -> Result<Value, HttpError> {
        let robot = self.robot(key)?;
        let ble_id = robot["bleId"].as_str().filter(|_| robot["canProvisionWifi"] == true).map(str::to_string).ok_or_else(|| {
            HttpError::conflict(format!(
                "{} was only seen on the network (no Bluetooth address), so its Wi-Fi can't be set from here",
                robot["name"].as_str().unwrap_or(key)
            ))
        })?;
        let ssid = ssid.trim().to_string();
        if ssid.is_empty() {
            return Err(HttpError::bad("ssid is required"));
        }
        let country = country.trim().to_uppercase();
        if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(HttpError::bad("country must be a two-letter code, e.g. US"));
        }
        if self.wifi_state()["status"] == "running" {
            return Err(HttpError::conflict("a Wi-Fi provisioning is already running — POST api/wifi/cancel first"));
        }
        let warning = self.host_ssid().filter(|host| host != &ssid).map(|host| {
            format!("This computer is on “{host}”, not “{ssid}”: a dog on “{ssid}” won't be reachable or get an IP here unless this computer joins “{ssid}” too.")
        });
        let name = robot["name"].as_str().unwrap_or(key).to_string();
        if dry_run {
            let steps = vec![
                format!("would connect over Bluetooth to {name} ({ble_id})"),
                "would handshake, read the serial and switch it to station mode".to_string(),
                format!("would send SSID “{ssid}”, a {}-character password and country {country}", password.chars().count()),
            ];
            self.set_wifi(|wifi| *wifi = json!({ "status": "dry-run", "robot": key, "ssid": ssid, "country": country, "log": steps, "dryRun": true, "warning": warning }));
            return Ok(
                json!({ "dryRun": true, "sent": false, "robot": key, "bleId": ble_id, "ssid": ssid, "country": country, "steps": steps, "warning": warning }),
            );
        }
        self.set_wifi(|wifi| *wifi = json!({ "status": "running", "robot": key, "ssid": ssid, "country": country, "log": [format!("Connecting {name} → “{ssid}” …")], "warning": warning }));
        let app = self.clone();
        let (ssid_task, password, country_task) = (ssid.clone(), password.to_string(), country.clone());
        let robot_serial = robot["serial"].as_str().map(str::to_string);
        let task = tokio::spawn(async move {
            if app.mock {
                for step in ["handshake", "read serial", "init STA mode", "set SSID", "set password", "set country"] {
                    app.wifi_log(step.to_string());
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                // the mock robot shows up on the network a few seconds later (a real Go2: ~30 s)
                let serial = robot_serial.clone().unwrap_or_else(|| "MOCKSERIAL".to_string());
                app.mock_joined.lock().unwrap().insert(serial.clone(), now_ms() + MOCK_JOIN_MS);
                return Ok(Some(serial));
            }
            let peripheral =
                app.registry.lock().await.get(&ble_id).cloned().ok_or_else(|| format!("device {ble_id} not found — scan again first"))?;
            let mut last_error = String::from("provisioning failed");
            for attempt in 0..3 {
                let log_app = app.clone();
                match ble::provision_wifi(peripheral.clone(), &ssid_task, &password, &country_task, 3, move |msg| log_app.wifi_log(msg))
                    .await
                {
                    Ok(serial) => return Ok(serial),
                    Err(err) => {
                        last_error = err;
                        app.wifi_log(format!("attempt {} failed: {last_error}", attempt + 1));
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            Err(last_error)
        });
        *self.wifi_task.lock().await = Some(task.abort_handle());
        let outcome = task.await;
        *self.wifi_task.lock().await = None;
        match outcome {
            Ok(Ok(serial)) => {
                self.set_wifi(|wifi| {
                    wifi["status"] = json!("ok");
                    wifi["serial"] = json!(serial);
                });
                self.wifi_log(format!(
                    "✓ Wi-Fi credentials accepted{}",
                    serial.as_ref().map(|s| format!(" — serial {s}")).unwrap_or_default()
                ));
                Ok(
                    json!({ "ok": true, "sent": true, "robot": key, "serial": serial, "ssid": ssid, "warning": warning, "next": "the robot joins the network in ~30 s; scan again to see its IP" }),
                )
            }
            Ok(Err(err)) => {
                self.set_wifi(|wifi| {
                    wifi["status"] = json!("error");
                    wifi["error"] = json!(err);
                });
                Err(HttpError::upstream(err))
            }
            Err(_) => Err(HttpError::conflict("cancelled")),
        }
    }

    pub async fn cancel_wifi(&self) -> Result<Value, HttpError> {
        let Some(task) = self.wifi_task.lock().await.take() else {
            return Err(HttpError::conflict("no Wi-Fi provisioning is running"));
        };
        task.abort();
        self.set_wifi(|wifi| wifi["status"] = json!("cancelled"));
        self.wifi_log("Cancelled.".to_string());
        Ok(json!({ "cancelled": true }))
    }

    pub fn clear_wifi(&self) -> Result<Value, HttpError> {
        if self.wifi_state()["status"] == "running" {
            return Err(HttpError::conflict("a Wi-Fi provisioning is running — POST api/wifi/cancel first"));
        }
        self.set_wifi(|wifi| *wifi = json!({ "status": "idle" }));
        Ok(self.wifi_state())
    }

    // ── Unitree accounts (AES keys) ──

    /// Every saved AES key with the dog it belongs to (for the page's "download keys" button only; a private endpoint).
    pub fn aes_keys_import(&self, entries: &Value) -> Result<Value, HttpError> {
        let entries = entries.as_array().ok_or_else(|| HttpError::bad("keys must be an array"))?;
        if entries.is_empty() || entries.len() > 10000 {
            return Err(HttpError::bad("provide 1–10000 keys"));
        }
        let mut validated = Vec::new();
        for entry in entries {
            let key = entry["key"]
                .as_str()
                .filter(|k| !k.trim().is_empty())
                .or_else(|| entry["serial"].as_str().filter(|k| !k.trim().is_empty()))
                .ok_or_else(|| HttpError::bad("each key needs key or serial"))?;
            let aes = entry["aesKey"].as_str().ok_or_else(|| HttpError::bad("each key needs aesKey"))?.trim().to_lowercase();
            crate::robot_rtc::parse_aes_key(&aes).map_err(HttpError::bad)?;
            validated.push((key.to_string(), aes, entry["name"].as_str().filter(|n| !n.trim().is_empty()).map(str::to_string)));
        }
        let (keys, names) = {
            let mut state = self.state.lock().unwrap();
            for (key, aes, name) in validated {
                state.aes_keys.insert(key.clone(), aes);
                if let Some(name) = name {
                    state.names.insert(key, name);
                }
            }
            (json!(state.aes_keys), json!(state.names))
        };
        self.save(AES_KEYS_FILE, keys, true);
        self.save(NAMES_FILE, names, false);
        self.publish_robots();
        Ok(json!({ "imported": entries.len() }))
    }

    pub fn aes_keys_export(&self) -> Value {
        let state = self.state.lock().unwrap();
        let alias = |sn: &str| {
            state
                .accounts
                .values()
                .flat_map(|a| a.robots.iter())
                .find(|r| r["sn"] == sn)
                .and_then(|r| r["alias"].as_str())
                .filter(|a| !a.is_empty())
        };
        let keys: Vec<Value> = state
            .aes_keys
            .iter()
            .map(|(key, aes_key)| {
                let device = state.devices.get(key);
                let serial = device
                    .and_then(|d| d["serial"].as_str())
                    .or_else(|| (!valid_ipv4(key) && !key.contains([':', '-'])).then_some(key.as_str()));
                let name = state
                    .names
                    .get(key)
                    .map(String::as_str)
                    .or_else(|| device.and_then(|d| d["name"].as_str()))
                    .or_else(|| serial.and_then(alias));
                json!({ "key": key, "name": name, "serial": serial, "aesKey": aes_key })
            })
            .collect();
        json!({ "exportedAt": now_ms(), "keys": keys })
    }

    pub fn accounts(&self) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        state
            .accounts
            .iter()
            .map(
                |(email, a)| json!({ "email": email, "lastPull": a.last_pull, "pulling": a.pulling, "error": a.error, "robots": a.robots }),
            )
            .collect()
    }

    fn persist_accounts(&self) {
        let data = {
            let state = self.state.lock().unwrap();
            let mut out = serde_json::Map::new();
            for (email, a) in &state.accounts {
                out.insert(email.clone(), json!({ "password": a.password, "lastPull": a.last_pull, "robots": a.robots, "error": a.error }));
            }
            Value::Object(out)
        };
        self.save(ACCOUNTS_FILE, data, true);
        let accounts = self.accounts();
        self.publish(json!({ "type": "accounts", "accounts": accounts }));
    }

    pub async fn add_account(self: &Arc<Self>, email: &str, password: &str) -> Result<Value, HttpError> {
        let email = email.trim().to_lowercase();
        if !email.contains('@') {
            return Err(HttpError::bad("email must be an email address"));
        }
        if password.is_empty() {
            return Err(HttpError::bad("password is required"));
        }
        {
            let mut state = self.state.lock().unwrap();
            let account = state.accounts.entry(email.clone()).or_default();
            account.password = password.to_string();
            account.error = None;
        }
        self.persist_accounts();
        self.pull_account(&email).await
    }

    /// Signs in to the Unitree cloud (every region and app) and saves the AES key of every robot bound to the account.
    pub async fn pull_account(self: &Arc<Self>, email: &str) -> Result<Value, HttpError> {
        let email = email.trim().to_lowercase();
        let password = {
            let mut state = self.state.lock().unwrap();
            let account = state.accounts.get_mut(&email).ok_or_else(|| HttpError::not_found(format!("no saved account {email}")))?;
            if account.pulling {
                return Err(HttpError::conflict(format!("{email} is already being pulled")));
            }
            account.pulling = true;
            account.password.clone()
        };
        self.persist_accounts();
        let result = if self.mock {
            Ok(vec![crate::cloud::BoundRobot {
                sn: "MOCK0000GO2A0001".into(),
                alias: "mock dog".into(),
                key: "00112233445566778899aabbccddeeff".into(),
            }])
        } else {
            crate::cloud::fetch_bound_robots(&email, &password).await
        };
        let outcome = {
            let mut state = self.state.lock().unwrap();
            if let Ok(robots) = &result {
                for robot in robots {
                    if crate::robot_rtc::parse_aes_key(&robot.key).is_ok() {
                        state.aes_keys.insert(robot.sn.clone(), robot.key.to_lowercase());
                    }
                }
            }
            let account = state.accounts.entry(email.clone()).or_default();
            account.pulling = false;
            match &result {
                Ok(robots) => {
                    account.robots = robots
                        .iter()
                        .map(|r| json!({ "sn": r.sn, "alias": r.alias, "hasKey": crate::robot_rtc::parse_aes_key(&r.key).is_ok() }))
                        .collect();
                    account.error = None;
                    account.last_pull = Some(now_ms());
                }
                Err(err) => account.error = Some(err.clone()),
            }
            json!(state.aes_keys)
        };
        self.save(AES_KEYS_FILE, outcome, true);
        self.persist_accounts();
        self.publish_robots();
        match result {
            Ok(_) => Ok(self.accounts().into_iter().find(|a| a["email"] == email).unwrap_or(Value::Null)),
            Err(err) => Err(HttpError::upstream(err)),
        }
    }

    pub fn remove_account(&self, email: &str) -> Result<Value, HttpError> {
        let email = email.trim().to_lowercase();
        if self.state.lock().unwrap().accounts.remove(&email).is_none() {
            return Err(HttpError::not_found(format!("no saved account {email}")));
        }
        self.persist_accounts();
        Ok(json!({ "removed": email }))
    }

    /// Finds the robot a drive session is for (by key) or the one at an IP, if any.
    pub fn resolve_target(&self, key: Option<&str>, ip: Option<&str>) -> Result<(Option<String>, String, String), HttpError> {
        if let Some(key) = key.filter(|k| !k.is_empty()) {
            let robot = self.robot(key)?;
            let ip = ip.filter(|ip| !ip.is_empty()).map(str::to_string).or_else(|| robot["ip"].as_str().map(str::to_string)).ok_or_else(
                || {
                    HttpError::conflict(format!(
                        "{key} has no known IP — scan again once it's on the network, or set one with PUT api/robots/{key}/ip"
                    ))
                },
            )?;
            return Ok((Some(key.to_string()), ip, robot["name"].as_str().unwrap_or(key).to_string()));
        }
        let ip = ip.filter(|ip| !ip.is_empty()).ok_or_else(|| HttpError::bad("give robot (a key from GET api/robots) or ip"))?;
        if !valid_ipv4(ip) {
            return Err(HttpError::bad(format!("{ip} isn't an IPv4 address")));
        }
        let found = self.robots().into_iter().find(|robot| robot["ip"] == ip);
        Ok(match found {
            Some(robot) => (robot["key"].as_str().map(str::to_string), ip.to_string(), robot["name"].as_str().unwrap_or(ip).to_string()),
            None => (None, ip.to_string(), format!("Go2 at {ip}")),
        })
    }
}

/// How long after the mock's Wi-Fi provisioning the robot is seen on the network.
pub const MOCK_JOIN_MS: u64 = 4000;

/// The mock's two robots: one already on the network, one only over Bluetooth (a new Go2) until it's given Wi-Fi
/// (`joined`: the serials that have joined since). Found one after another, as a real scan finds them.
async fn mock_scan(sink: Sink, timeout_secs: f64, joined: &[String], sweep: discovery::SweepMode, stop: &discovery::Stop) {
    let pause = Duration::from_secs_f64((timeout_secs / 10.0).min(0.6));
    sink(json!({ "type": "scan_start" }));
    let lan = json!({ "type": "sweep", "iface": "mock0", "ip": "192.0.2.2", "subnet": "192.0.2.0/24", "fullHosts": 254 });
    let with = |extra: Value| {
        let mut event = lan.clone();
        event.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        event
    };
    if sweep != discovery::SweepMode::Off {
        sink(with(
            json!({ "status": "running", "phase": "subnet", "swept": "192.0.2.0/24", "partial": false, "sent": 127, "total": 253, "alive": 3 }),
        ));
    }
    tokio::time::sleep(pause).await;
    if stop.flag.load(std::sync::atomic::Ordering::Relaxed) {
        sink(with(json!({ "status": "cancelled", "phase": "done", "swept": null, "partial": true, "note": "stopped" })));
        sink(json!({ "type": "scan_done", "count": 0 }));
        return;
    }
    let second = "MOCK0000GO2A0002";
    let second_ip = joined.iter().any(|s| s == second).then_some("192.0.2.11");
    sink(
        json!({ "type": "device", "serial": second, "name": "Go2_MOCK2", "ble_mac": "mock-ble-2", "ip": second_ip, "lan_mac": second_ip.map(|_| "94:ba:06:00:00:02"), "arp_only": false, "matched": if second_ip.is_some() { "ble+arp" } else { "ble" } }),
    );
    tokio::time::sleep(pause).await;
    sink(
        json!({ "type": "device", "serial": "MOCK0000GO2A0001", "name": "Go2_MOCK1", "ble_mac": "mock-ble-1", "ip": "192.0.2.10", "lan_mac": "94:ba:06:00:00:01", "arp_only": false, "matched": "ble+arp" }),
    );
    tokio::time::sleep(pause).await;
    if sweep != discovery::SweepMode::Off {
        sink(with(
            json!({ "status": "done", "phase": "done", "swept": "192.0.2.0/24", "partial": false, "method": "ping", "note": "swept 192.0.2.0/24", "known": 0 }),
        ));
    }
    sink(json!({ "type": "scan_done", "count": 2 }));
}

async fn run(cmd: &str, args: &[&str]) -> String {
    match tokio::process::Command::new(cmd).args(args).stderr(std::process::Stdio::null()).output().await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}

async fn wifi_device() -> String {
    let ports = run("networksetup", &["-listallhardwareports"]).await;
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

async fn route_interface(host: &str) -> Option<String> {
    let out = run("route", &["-n", "get", host]).await;
    out.lines().find_map(|line| line.trim().strip_prefix("interface:").map(|v| v.trim().to_string()))
}

/// Best-effort current-Wi-Fi SSID, macOS + Linux; "" when it can't tell.
async fn detect_ssid() -> String {
    if cfg!(target_os = "macos") {
        let device = wifi_device().await;
        // `ipconfig getsummary` still prints the SSID where networksetup is gated behind Location Services
        let summary = run("ipconfig", &["getsummary", &device]).await;
        if let Some(ssid) = summary.lines().find_map(|line| line.trim().strip_prefix("SSID :").map(|v| v.trim().to_string())) {
            return ssid;
        }
        let ns = run("networksetup", &["-getairportnetwork", &device]).await;
        return ns.lines().find_map(|line| line.strip_prefix("Current Wi-Fi Network:").map(|v| v.trim().to_string())).unwrap_or_default();
    }
    let nm = run("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"]).await;
    if let Some(ssid) = nm.lines().find_map(|line| line.strip_prefix("yes:")) {
        return ssid.trim().to_string();
    }
    run("iwgetid", &["-r"]).await.trim().to_string()
}

/// Same private fleet file as Desktop/steam_install: SERIAL KEY ALIAS.
fn shared_aes_key(identity: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(std::path::PathBuf::from(home).join(".config/dimos/go2-keys")).ok()?;
    parse_shared_aes_key(&text, identity)
}

fn parse_shared_aes_key(text: &str, identity: &str) -> Option<String> {
    let rows: Vec<Vec<&str>> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| l.split_whitespace().collect())
        .filter(|r: &Vec<&str>| r.len() >= 2)
        .collect();
    let row = rows
        .iter()
        .find(|r| r[0] == identity)
        .or_else(|| rows.iter().find(|r| r.get(2).is_some_and(|alias| identity == *alias || identity.starts_with(&format!("{alias}_")))))?;
    crate::robot_rtc::parse_aes_key(row[1]).ok()?;
    Some(row[1].to_lowercase())
}

#[cfg(test)]
mod fleet_keys_tests {
    use super::*;
    #[test]
    fn shared_keys_match_serial_and_wifi_alias() {
        let text = "# private fleet\nSERIAL 0123456789abcdef0123456789abcdef Go2\nOTHER (empty) Broken\n";
        assert_eq!(parse_shared_aes_key(text, "SERIAL"), parse_shared_aes_key(text, "Go2_49077"));
        assert!(parse_shared_aes_key(text, "SERIAL").is_some());
        assert!(parse_shared_aes_key(text, "Go20_49077").is_none());
        assert!(parse_shared_aes_key(text, "Broken").is_none());
    }
    #[test]
    fn invalid_import_does_not_write_partial_keys() {
        let dir = std::env::temp_dir().join(format!("go2-import-{}", now_ms()));
        let app = App::new(dir.clone(), true);
        let before = app.aes_keys_export()["keys"].clone();
        assert!(app
            .aes_keys_import(&json!([
                {"serial":"NEW", "aesKey":"0123456789abcdef0123456789abcdef"},
                {"serial":"BAD", "aesKey":"invalid"}
            ]))
            .is_err());
        assert_eq!(app.aes_keys_export()["keys"], before);
        assert_eq!(
            app.aes_keys_import(&json!([{"serial":"NEW", "aesKey":"0123456789abcdef0123456789abcdef", "name":"Go2_49077"}])).unwrap()
                ["imported"],
            1
        );
        assert_eq!(app.aes_key_for("NEW"), "0123456789abcdef0123456789abcdef");
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The keys in a dimos go2-keys file: "SERIAL KEY ALIAS" lines, skipping comments and "(empty)" keys.
pub fn fleet_keys(text: &str) -> Vec<crate::robot_rtc::AesKey> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (serial, key) = (fields.next()?, fields.next()?);
            let alias = fields.next().unwrap_or("");
            crate::robot_rtc::parse_aes_key(key).ok()?;
            Some((format!("go2-keys {serial} {alias}").trim_end().to_string(), key.to_lowercase()))
        })
        .collect()
}
