// Scan orchestration: Bluetooth, LAN discovery (multicast, plus broadcast on macOS) and the ARP sweep run together and
// merge into one card per robot (keyed by serial). Started as a port of go2_helper.py's do_scan; the sweep and the
// Bluetooth-MAC join come from find-go2.sh:
//   1. Bluetooth: name (Go2_xxxxx), serial (in its advertisement) and, on Linux, its Bluetooth MAC
//   2. ARP: re-ping the IPs dogs were seen at before, then sweep the subnet (sweep.rs), and read the neighbour table
//   3. join: Wi-Fi MAC = Bluetooth MAC, last byte minus 1. Unitree-prefixed MACs nothing claimed show as "possible"
//   4. remember where dogs were seen (`seen` events; app.rs keeps them) for the next scan's quick path
// Office Wi-Fi drops 231.1.1.1, so LAN discovery stays silent there; the ARP path doesn't need it.
//
// All merge state lives in this one task; producers feed it over channels, so the maps need no locking.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};
use tokio::time::{interval_at, Duration as TokioDuration, Instant};

use crate::arp::{ble_to_wifi_mac, go2_alive, is_unitree_oui, read_neighbors};
use crate::ble::{scan_ble, BleDevice, Registry};
use crate::lan::{discover_broadcast, discover_multicast, LanDevice};
use crate::sweep::{local_lan, sweep, sweep_order, Lan, Subnet, QUICK_MAX_HOSTS, WIDEST_PREFIX};
use btleplug::api::Central;
use btleplug::platform::Adapter;

#[derive(Default, Clone)]
struct Record {
    serial: Option<String>,
    name: Option<String>,
    /// btleplug's id for it (a MAC on Linux, a UUID on macOS): what provisioning reconnects by
    ble_mac: Option<String>,
    /// the real Bluetooth MAC (Linux only)
    ble_hw_mac: Option<String>,
    ip: Option<String>,
    /// its Wi-Fi MAC
    lan_mac: Option<String>,
    arp_only: bool,
    /// how its IP was found: "lan" (answered discovery), "arp" (Bluetooth MAC joined to the ARP table), "guess" (the only
    /// Go2 on Bluetooth and the only unclaimed Unitree MAC on the network)
    via: Option<&'static str>,
}

impl Record {
    /// "ble+arp", "ble+lan", "lan", "oui" (a Unitree MAC only), "ble" (on Bluetooth, not on this network) or "guess"
    fn matched(&self) -> &'static str {
        match (self.arp_only, self.ip.is_some(), self.via, self.ble_mac.is_some()) {
            (true, ..) => "oui",
            (_, false, ..) => "ble",
            (_, true, Some("guess"), _) => "guess",
            (_, true, Some("arp"), _) => "ble+arp",
            (_, true, _, true) => "ble+lan",
            _ => "lan",
        }
    }

    fn to_event(&self) -> Value {
        json!({
            "type": "device",
            "serial": self.serial,
            "name": self.name,
            "ble_mac": self.ble_mac,
            "ble_hw_mac": self.ble_hw_mac,
            "ip": self.ip,
            "lan_mac": self.lan_mac,
            "arp_only": self.arp_only,
            "matched": self.matched(),
        })
    }
}

/// Where discovery reports `device` / `drop` / `sweep` / `seen` / `scan_start` / `scan_done` / `warn` events (app.rs
/// folds them into state).
pub type Sink = Arc<dyn Fn(Value) + Send + Sync>;

/// One sighting from any source; the fields it doesn't know stay None.
#[derive(Default)]
struct Sighting {
    serial: Option<String>,
    name: Option<String>,
    ble_mac: Option<String>,
    ble_hw_mac: Option<String>,
    ip: Option<String>,
    lan_mac: Option<String>,
    via: Option<&'static str>,
}

struct Merger {
    emit: Sink,
    merged: HashMap<String, Record>,
    mac_key: HashMap<String, String>,
    ip_key: HashMap<String, String>,
    /// every (ip → mac) the neighbour table showed this scan: a big sweep can push an entry out before the next read
    neighbors: HashMap<Ipv4Addr, String>,
    /// OUI-only candidates already probed (ip → a Go2 signaling port answered)
    probed: HashMap<Ipv4Addr, bool>,
    seen_reported: HashSet<Ipv4Addr>,
    /// distinct robots seen over Bluetooth, for the sweep's "the remembered IPs found them all" check
    ble_seen: Arc<AtomicUsize>,
}

impl Merger {
    fn new(emit: Sink, ble_seen: Arc<AtomicUsize>) -> Self {
        Merger {
            emit,
            merged: HashMap::new(),
            mac_key: HashMap::new(),
            ip_key: HashMap::new(),
            neighbors: HashMap::new(),
            probed: HashMap::new(),
            seen_reported: HashSet::new(),
            ble_seen,
        }
    }

    fn emit_drop(&self, key: &str) {
        (self.emit)(json!({ "type": "drop", "key": key }));
    }

    fn upsert(&mut self, sighting: Sighting) {
        let Sighting { serial, name, ble_mac, ble_hw_mac, ip, lan_mac, via } = sighting;
        // Key by serial when known; before a serial is recovered key by BLE
        // address, then migrate the address-only card onto the serial key.
        let key = if let Some(serial) = &serial {
            format!("s:{serial}")
        } else if let Some(mac) = &ble_mac {
            match self.mac_key.get(mac) {
                Some(existing) if existing.starts_with("s:") => existing.clone(),
                _ => format!("b:{mac}"),
            }
        } else {
            "b:".to_string()
        };

        // Fold a prior address-only card into the serial card, dropping the stale one.
        if serial.is_some() {
            if let Some(mac) = &ble_mac {
                if let Some(old_key) = self.mac_key.get(mac).cloned() {
                    if old_key != key && self.merged.contains_key(&old_key) {
                        let old = self.merged.remove(&old_key).unwrap();
                        let base = self.merged.entry(key.clone()).or_insert_with(|| Record { serial: serial.clone(), ..Default::default() });
                        base.name = base.name.take().or(old.name);
                        base.ble_mac = base.ble_mac.take().or(old.ble_mac);
                        base.ble_hw_mac = base.ble_hw_mac.take().or(old.ble_hw_mac);
                        base.ip = base.ip.take().or(old.ip);
                        base.lan_mac = base.lan_mac.take().or(old.lan_mac);
                        base.via = base.via.or(old.via);
                        // The dropped card's frontend key was its ble_mac (no serial).
                        self.emit_drop(old_key.split_once(':').map(|(_, rest)| rest).unwrap_or(""));
                    }
                }
            }
        }

        let rec = self.merged.entry(key.clone()).or_insert_with(|| Record { serial: serial.clone(), ..Default::default() });
        rec.serial = serial.or(rec.serial.take());
        rec.name = name.or(rec.name.take());
        rec.ble_mac = ble_mac.clone().or(rec.ble_mac.take());
        rec.ble_hw_mac = ble_hw_mac.or(rec.ble_hw_mac.take());
        rec.lan_mac = lan_mac.or(rec.lan_mac.take());

        if let Some(ip) = &ip {
            rec.ip = Some(ip.clone());
            rec.via = via.or(rec.via);
            self.ip_key.insert(ip.clone(), key.clone());
            let arp_key = format!("a:{ip}");
            if arp_key != key && self.merged.remove(&arp_key).is_some() {
                self.emit_drop(ip);
            }
        }

        if let Some(mac) = &ble_mac {
            if self.mac_key.insert(mac.clone(), key.clone()).is_none() {
                self.ble_seen.store(self.mac_key.len(), Ordering::Relaxed);
            }
        }

        (self.emit)(self.merged[&key].to_event());
    }

    fn arp_upsert(&mut self, ip: &str, mac: &str) {
        // Already known via BLE/LAN with a real identity — just attach the mac.
        if let Some(key) = self.ip_key.get(ip).cloned() {
            if let Some(rec) = self.merged.get_mut(&key) {
                if rec.lan_mac.is_none() {
                    rec.lan_mac = Some(mac.to_string());
                    (self.emit)(rec.to_event());
                }
            }
            return;
        }
        let key = format!("a:{ip}");
        if self.merged.get(&key).is_some_and(|existing| existing.lan_mac.as_deref() == Some(mac)) {
            return; // unchanged — don't spam the panel
        }
        let rec = Record { ip: Some(ip.to_string()), lan_mac: Some(mac.to_string()), arp_only: true, ..Default::default() };
        (self.emit)(rec.to_event());
        self.merged.insert(key, rec);
    }

    fn arp_drop(&mut self, ip: &str) {
        if self.merged.remove(&format!("a:{ip}")).is_some() {
            self.emit_drop(ip);
        }
    }

    fn ip_known(&self, ip: &str) -> bool {
        self.ip_key.contains_key(ip)
    }

    /// Read the neighbour table and fold it in: MACs onto known IPs, Bluetooth MACs joined to Wi-Fi MACs, and
    /// Unitree-prefixed MACs nothing claimed (each confirmed by a Go2 signaling port answering) as possible Go2s.
    async fn arp_tick(&mut self) {
        for (ip, mac) in read_neighbors().await {
            self.neighbors.insert(ip, mac);
        }
        let by_mac: HashMap<String, Ipv4Addr> = self.neighbors.iter().map(|(ip, mac)| (mac.clone(), *ip)).collect();

        // the join: a Bluetooth MAC (Linux) names its Wi-Fi MAC
        let joins: Vec<Sighting> = self
            .merged
            .values()
            .filter(|rec| !rec.arp_only)
            .filter_map(|rec| {
                let wifi = ble_to_wifi_mac(rec.ble_hw_mac.as_deref()?)?;
                let ip = by_mac.get(&wifi)?.to_string();
                (rec.ip.as_deref() != Some(&ip)).then(|| Sighting {
                    serial: rec.serial.clone(),
                    ble_mac: rec.ble_mac.clone(),
                    ip: Some(ip),
                    lan_mac: Some(wifi),
                    via: Some("arp"),
                    ..Default::default()
                })
            })
            .collect();
        for join in joins {
            self.upsert(join);
        }

        let pairs: Vec<(Ipv4Addr, String)> = self.neighbors.iter().map(|(ip, mac)| (*ip, mac.clone())).collect();
        for (ip, mac) in &pairs {
            let ip_text = ip.to_string();
            if self.ip_known(&ip_text) {
                self.arp_upsert(&ip_text, mac);
            }
            if is_unitree_oui(mac) && self.seen_reported.insert(*ip) {
                (self.emit)(json!({ "type": "seen", "ip": ip_text, "mac": mac }));
            }
        }

        let candidates: Vec<(Ipv4Addr, String)> =
            pairs.into_iter().filter(|(ip, mac)| is_unitree_oui(mac) && !self.ip_known(&ip.to_string()) && !self.probed.contains_key(ip)).collect();
        let alive = futures::future::join_all(candidates.iter().map(|(ip, _)| go2_alive_v4(*ip))).await;
        for ((ip, mac), alive) in candidates.into_iter().zip(alive) {
            self.probed.insert(ip, alive);
            if alive {
                self.arp_upsert(&ip.to_string(), &mac);
            } else {
                self.arp_drop(&ip.to_string());
            }
        }
    }

    /// No Bluetooth MAC to join by (macOS): when exactly one Go2 on Bluetooth has no IP and exactly one unclaimed
    /// Unitree MAC answers on the network, they are probably the same dog. Labelled a guess.
    fn guess_by_elimination(&mut self) {
        let unplaced: Vec<&Record> = self.merged.values().filter(|r| !r.arp_only && r.ip.is_none() && r.ble_hw_mac.is_none() && r.ble_mac.is_some()).collect();
        let loose: Vec<&Record> = self.merged.values().filter(|r| r.arp_only).collect();
        if let ([dog], [possible]) = (unplaced.as_slice(), loose.as_slice()) {
            let sighting = Sighting {
                serial: dog.serial.clone(),
                ble_mac: dog.ble_mac.clone(),
                ip: possible.ip.clone(),
                lan_mac: possible.lan_mac.clone(),
                via: Some("guess"),
                ..Default::default()
            };
            self.upsert(sighting);
        }
    }
}

async fn go2_alive_v4(ip: Ipv4Addr) -> bool {
    go2_alive(&ip.to_string()).await
}

fn spawn_lan_loop(tx: mpsc::Sender<LanDevice>, broadcast: bool, tick: Duration, probe_timeout: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let devices = tokio::task::spawn_blocking(move || if broadcast { discover_broadcast(probe_timeout) } else { discover_multicast(probe_timeout) })
                .await
                .unwrap_or_default();
            for device in devices {
                if tx.send(device).await.is_err() {
                    return;
                }
            }
            tokio::time::sleep(tick.saturating_sub(probe_timeout)).await;
        }
    })
}

// ── the sweep ──

/// How much of the network to sweep for ARP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepMode {
    /// the IPs dogs were seen at before; then, unless they account for every dog on Bluetooth, the subnet when it is
    /// small (≤ 1024 addresses) or else the /24 around this computer, widened to the whole subnet when a dog on
    /// Bluetooth is still missing (`widen`)
    Quick,
    /// the remembered IPs, then the whole subnet (up to a /16): a /17 takes ~1 min on macOS, ~3 min on Linux (its neighbour table caps the rate)
    Full,
    /// only the remembered IPs
    Known,
    Off,
}

impl SweepMode {
    pub fn parse(text: &str) -> Option<SweepMode> {
        match text {
            "quick" | "" => Some(SweepMode::Quick),
            "full" => Some(SweepMode::Full),
            "known" => Some(SweepMode::Known),
            "off" => Some(SweepMode::Off),
            _ => None,
        }
    }
}

/// Stops a running scan (POST api/scan/stop): the sweep at its next batch, the rest right away.
#[derive(Default)]
pub struct Stop {
    pub flag: AtomicBool,
    pub notify: Notify,
}

impl Stop {
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.notify.notify_one();
    }
}

fn sweep_event(lan: &Lan, status: &str, phase: &str, swept: Option<Subnet>, extra: Value) -> Value {
    let widest = lan.subnet.at_most(lan.ip, WIDEST_PREFIX);
    let mut event = json!({
        "type": "sweep",
        "status": status,
        "phase": phase,
        "iface": lan.iface,
        "ip": lan.ip.to_string(),
        "subnet": lan.subnet.to_string(),
        "swept": swept.map(|s| s.to_string()),
        // whether the whole (up to /16) subnet was covered; when not, `fullHosts` is what "sweep everything" would probe
        "partial": swept.map_or(true, |s| s.prefix > widest.prefix),
        "fullHosts": widest.host_count(),
    });
    if let (Some(event), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
        event.extend(extra.clone());
    }
    event
}

/// `widen`: in quick mode, sweep the whole subnet too when a dog seen on Bluetooth is still missing from it (app.rs
/// allows that once every few minutes, so the guide's repeated rescans don't sweep a big network back to back).
async fn run_sweep(mode: SweepMode, known: Vec<Ipv4Addr>, widen: bool, ble_seen: Arc<AtomicUsize>, stop: Arc<Stop>, emit: Sink) {
    let Some(lan) = local_lan().await else {
        emit(json!({ "type": "sweep", "status": "error", "error": "no network interface with an IPv4 address" }));
        return;
    };
    let iface = Some(lan.iface.clone());
    let known: Vec<Ipv4Addr> = known.into_iter().filter(|ip| lan.subnet.contains(*ip) && *ip != lan.ip).collect();

    let progress = |phase: &'static str, swept: Option<Subnet>| {
        let emit = emit.clone();
        let lan = lan.clone();
        move |p: crate::sweep::Progress| {
            emit(sweep_event(&lan, "running", phase, swept, json!({ "sent": p.sent, "total": p.total, "alive": p.alive })));
        }
    };
    let mut method = None;
    let mut rate = 0;
    if !known.is_empty() {
        match sweep(&known, iface.as_deref(), &stop.flag, progress("known", None)).await {
            Ok(swept) => {
                method = Some(swept.method);
                rate = swept.rate;
            }
            Err(err) => {
                emit(sweep_event(&lan, "error", "known", None, json!({ "error": format!("can't probe: {err}") })));
                return;
            }
        }
    }
    let hits = read_neighbors().await.iter().filter(|(ip, mac)| is_unitree_oui(mac) && lan.subnet.contains(*ip)).count();
    let found_all = hits > 0 && hits >= ble_seen.load(Ordering::Relaxed);
    let target = match mode {
        SweepMode::Full => Some(lan.subnet.at_most(lan.ip, WIDEST_PREFIX)),
        SweepMode::Quick if !found_all => {
            Some(if lan.subnet.host_count() <= QUICK_MAX_HOSTS { lan.subnet } else { lan.subnet.at_most(lan.ip, 24) })
        }
        _ => None,
    };
    let mut swept_subnet = None;
    let mut cancelled = stop.flag.load(Ordering::Relaxed);
    if let (Some(target), false) = (target, cancelled) {
        let order = sweep_order(target, lan.ip);
        match sweep(&order, iface.as_deref(), &stop.flag, progress("subnet", Some(target))).await {
            Ok(swept) => {
                method = Some(swept.method);
                rate = swept.rate;
                cancelled = swept.cancelled;
                swept_subnet = (!swept.cancelled).then_some(target);
            }
            Err(err) => {
                emit(sweep_event(&lan, "error", "subnet", None, json!({ "error": format!("can't probe: {err}") })));
                return;
            }
        }
    }
    // a dog on Bluetooth still has no Unitree MAC on the network: the quick sweep missed it, widen to the whole subnet
    let widest = lan.subnet.at_most(lan.ip, WIDEST_PREFIX);
    let mut widened = false;
    if let (true, Some(narrow), false) = (mode == SweepMode::Quick && widen, swept_subnet, cancelled) {
        let hits = read_neighbors().await.iter().filter(|(ip, mac)| is_unitree_oui(mac) && lan.subnet.contains(*ip)).count();
        if narrow != widest && ble_seen.load(Ordering::Relaxed) > hits {
            let rest: Vec<Ipv4Addr> = sweep_order(widest, lan.ip).into_iter().filter(|ip| !narrow.contains(*ip)).collect();
            if let Ok(swept) = sweep(&rest, iface.as_deref(), &stop.flag, progress("widen", Some(widest))).await {
                cancelled = swept.cancelled;
                swept_subnet = (!swept.cancelled).then_some(widest);
                widened = true;
            }
        }
    }
    let note = match (cancelled, target, found_all) {
        (true, ..) => "stopped".to_string(),
        (_, Some(_), _) if widened => format!("swept {widest} (a dog on Bluetooth wasn't in the quick sweep)"),
        (_, None, true) => format!("the {} remembered IP{} found every dog on Bluetooth", known.len(), if known.len() == 1 { "" } else { "s" }),
        (_, None, false) => "remembered IPs only".to_string(),
        (_, Some(target), _) => format!("swept {target}"),
    };
    emit(sweep_event(
        &lan,
        if cancelled { "cancelled" } else { "done" },
        "done",
        swept_subnet,
        json!({ "method": method.map(|m| m.label()), "rate": rate, "known": known.len(), "note": note }),
    ));
}

pub struct ScanOptions {
    pub timeout_secs: f64,
    pub sweep: SweepMode,
    /// IPs dogs were seen at before (the sweep's quick path)
    pub known: Vec<Ipv4Addr>,
    /// quick mode may widen to the whole subnet when a dog on Bluetooth wasn't found (see run_sweep)
    pub widen: bool,
    pub stop: Arc<Stop>,
}

pub async fn do_scan(adapter: Option<Adapter>, registry: Registry, options: ScanOptions, emit: Sink) {
    emit(json!({ "type": "scan_start" }));
    let ble_seen = Arc::new(AtomicUsize::new(0));
    let mut merger = Merger::new(emit.clone(), ble_seen.clone());

    let (ble_tx, mut ble_rx) = mpsc::channel::<BleDevice>(64);
    let ble_task;
    // Keep the sender alive when there's no adapter so ble_rx pends (instead of
    // returning None forever and busy-looping the select).
    let _ble_keepalive;
    match adapter.clone() {
        Some(ble_adapter) => {
            let emit = emit.clone();
            ble_task = Some(tokio::spawn(async move {
                if let Err(err) = scan_ble(ble_adapter, registry, ble_tx).await {
                    let why = crate::ble::bluetooth_off_reason().unwrap_or(err);
                    emit(json!({ "type": "warn", "msg": format!("ble: {why}") }));
                }
            }));
            _ble_keepalive = None;
        }
        None => {
            ble_task = None;
            _ble_keepalive = Some(ble_tx);
        }
    }

    let (lan_tx, mut lan_rx) = mpsc::channel::<LanDevice>(64);
    let mut lan_tasks = vec![spawn_lan_loop(lan_tx.clone(), false, Duration::from_secs(2), Duration::from_millis(1500))];
    if cfg!(target_os = "macos") {
        lan_tasks.push(spawn_lan_loop(lan_tx.clone(), true, Duration::from_secs(2), Duration::from_millis(1500)));
    }
    drop(lan_tx);

    let mut sweep_task = if options.sweep == SweepMode::Off {
        None
    } else {
        Some(tokio::spawn(run_sweep(options.sweep, options.known, options.widen, ble_seen, options.stop.clone(), emit.clone())))
    };
    let mut arp_tick = interval_at(Instant::now() + TokioDuration::from_secs(1), TokioDuration::from_secs(1));
    let deadline = Instant::now() + TokioDuration::from_secs_f64(options.timeout_secs);
    let mut deadline_passed = false;

    // until the timeout has passed and the sweep is done, or POST api/scan/stop
    while !(deadline_passed && sweep_task.is_none()) {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline), if !deadline_passed => deadline_passed = true,
            _ = options.stop.notify.notified() => break,
            _ = async { sweep_task.as_mut().unwrap().await }, if sweep_task.is_some() => sweep_task = None,
            maybe = ble_rx.recv() => {
                if let Some(device) = maybe {
                    merger.upsert(Sighting { serial: device.serial, name: Some(device.name), ble_mac: Some(device.address), ble_hw_mac: device.mac, ..Default::default() });
                }
            }
            maybe = lan_rx.recv() => {
                if let Some(device) = maybe {
                    let lan_mac = device.mac.as_deref().and_then(crate::arp::norm_mac);
                    merger.upsert(Sighting { serial: Some(device.serial), ip: Some(device.ip), lan_mac, via: Some("lan"), ..Default::default() });
                }
            }
            _ = arp_tick.tick() => merger.arp_tick().await,
        }
    }
    if let Some(task) = sweep_task {
        task.abort();
    }
    if let Some(ble_task) = ble_task {
        ble_task.abort();
    }
    for task in lan_tasks {
        task.abort();
    }
    if let Some(adapter) = adapter {
        let _ = adapter.stop_scan().await;
    }
    merger.arp_tick().await;
    merger.guess_by_elimination();

    emit(json!({ "type": "scan_done", "count": merger.merged.len() }));
}
