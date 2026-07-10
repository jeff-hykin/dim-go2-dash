// Scan orchestration: run BLE + LAN (multicast, macOS broadcast) + ARP discovery
// concurrently and merge results by serial into one card per robot. Faithful port
// of do_scan / upsert / arp_upsert / arp_drop in go2_helper.py.
//
// All merge state lives in one task (this consumer loop); producer tasks feed it
// over channels, so the maps need no locking.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{interval_at, Duration as TokioDuration, Instant};

use crate::arp::{arp_scan, arp_sweep, go2_alive};
use crate::ble::{scan_ble, BleDevice, Registry};
use crate::emit;
use crate::lan::{discover_broadcast, discover_multicast, LanDevice};
use btleplug::api::Central;
use btleplug::platform::Adapter;

#[derive(Default, Clone)]
struct Record {
    serial: Option<String>,
    name: Option<String>,
    ble_mac: Option<String>,
    ip: Option<String>,
    lan_mac: Option<String>,
    arp_only: bool,
}

impl Record {
    fn blank(serial: Option<String>) -> Self {
        Record {
            serial,
            ..Default::default()
        }
    }

    fn to_event(&self) -> serde_json::Value {
        json!({
            "type": "device",
            "serial": self.serial,
            "name": self.name,
            "ble_mac": self.ble_mac,
            "ip": self.ip,
            "lan_mac": self.lan_mac,
            "arp_only": self.arp_only,
        })
    }
}

struct Merger {
    merged: HashMap<String, Record>,
    mac_key: HashMap<String, String>,
    ip_key: HashMap<String, String>,
}

fn emit_drop(key: &str) {
    emit(&json!({ "type": "drop", "key": key }));
}

impl Merger {
    fn new() -> Self {
        Merger {
            merged: HashMap::new(),
            mac_key: HashMap::new(),
            ip_key: HashMap::new(),
        }
    }

    fn upsert(
        &mut self,
        serial: Option<String>,
        name: Option<String>,
        ble_mac: Option<String>,
        ip: Option<String>,
        lan_mac: Option<String>,
    ) {
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
                        let base = self
                            .merged
                            .entry(key.clone())
                            .or_insert_with(|| Record::blank(serial.clone()));
                        if base.name.is_none() {
                            base.name = old.name;
                        }
                        if base.ble_mac.is_none() {
                            base.ble_mac = old.ble_mac;
                        }
                        if base.ip.is_none() {
                            base.ip = old.ip;
                        }
                        if base.lan_mac.is_none() {
                            base.lan_mac = old.lan_mac;
                        }
                        // The dropped card's frontend key was its ble_mac (no serial).
                        emit_drop(old_key.splitn(2, ':').nth(1).unwrap_or(""));
                    }
                }
            }
        }

        self.merged
            .entry(key.clone())
            .or_insert_with(|| Record::blank(serial.clone()));

        {
            let rec = self.merged.get_mut(&key).unwrap();
            if let Some(serial) = &serial {
                rec.serial = Some(serial.clone());
            }
            if let Some(name) = &name {
                rec.name = Some(name.clone());
            }
            if let Some(mac) = &ble_mac {
                rec.ble_mac = Some(mac.clone());
            }
        }

        if let Some(ip) = &ip {
            self.merged.get_mut(&key).unwrap().ip = Some(ip.clone());
            self.ip_key.insert(ip.clone(), key.clone());
            let arp_key = format!("a:{ip}");
            if arp_key != key && self.merged.contains_key(&arp_key) {
                self.merged.remove(&arp_key);
                emit_drop(ip);
            }
        }

        if let Some(lan_mac) = &lan_mac {
            self.merged.get_mut(&key).unwrap().lan_mac = Some(lan_mac.clone());
        }

        if let Some(mac) = &ble_mac {
            self.mac_key.insert(mac.clone(), key.clone());
        }

        emit(&self.merged.get(&key).unwrap().to_event());
    }

    fn arp_upsert(&mut self, ip: &str, mac: &str) {
        // Already known via BLE/LAN with a real identity — just attach the mac.
        if let Some(key) = self.ip_key.get(ip).cloned() {
            if let Some(rec) = self.merged.get_mut(&key) {
                if rec.lan_mac.is_none() {
                    rec.lan_mac = Some(mac.to_string());
                    emit(&rec.to_event());
                }
            }
            return;
        }
        let key = format!("a:{ip}");
        if let Some(existing) = self.merged.get(&key) {
            if existing.lan_mac.as_deref() == Some(mac) {
                return; // unchanged — don't spam the panel
            }
        }
        let rec = Record {
            ip: Some(ip.to_string()),
            lan_mac: Some(mac.to_string()),
            arp_only: true,
            ..Default::default()
        };
        emit(&rec.to_event());
        self.merged.insert(key, rec);
    }

    fn arp_drop(&mut self, ip: &str) {
        let key = format!("a:{ip}");
        if self.merged.remove(&key).is_some() {
            emit_drop(ip);
        }
    }

    fn ip_known(&self, ip: &str) -> bool {
        self.ip_key.contains_key(ip)
    }
}

fn spawn_lan_loop(
    tx: mpsc::Sender<LanDevice>,
    broadcast: bool,
    tick: Duration,
    probe_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let devices = tokio::task::spawn_blocking(move || {
                if broadcast {
                    discover_broadcast(probe_timeout)
                } else {
                    discover_multicast(probe_timeout)
                }
            })
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

pub async fn do_scan(adapter: Option<Adapter>, registry: Registry, timeout_secs: f64) {
    emit(&json!({ "type": "scan_start" }));
    let mut merger = Merger::new();

    let (ble_tx, mut ble_rx) = mpsc::channel::<BleDevice>(64);
    let ble_task;
    // Keep the sender alive when there's no adapter so ble_rx pends (instead of
    // returning None forever and busy-looping the select).
    let _ble_keepalive;
    match adapter.clone() {
        Some(ble_adapter) => {
            ble_task = Some(tokio::spawn(async move {
                if let Err(err) = scan_ble(ble_adapter, registry, ble_tx).await {
                    emit(&json!({ "type": "warn", "msg": format!("ble: {err}") }));
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
    let mut lan_tasks = vec![spawn_lan_loop(
        lan_tx.clone(),
        false,
        Duration::from_secs(2),
        Duration::from_millis(1500),
    )];
    if cfg!(target_os = "macos") {
        lan_tasks.push(spawn_lan_loop(
            lan_tx.clone(),
            true,
            Duration::from_secs(2),
            Duration::from_millis(1500),
        ));
    }
    drop(lan_tx);

    // Repopulate aged-out ARP entries up front, then poll the table.
    tokio::task::spawn_blocking(arp_sweep);
    let arp_start = Instant::now() + TokioDuration::from_secs(1);
    let mut arp_tick = interval_at(arp_start, TokioDuration::from_secs(2));

    let deadline = Instant::now() + TokioDuration::from_secs_f64(timeout_secs);

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = ble_rx.recv() => {
                match maybe {
                    Some(device) => merger.upsert(device.serial, Some(device.name), Some(device.address), None, None),
                    None => {}
                }
            }
            maybe = lan_rx.recv() => {
                if let Some(device) = maybe {
                    merger.upsert(Some(device.serial), None, None, Some(device.ip), device.mac);
                }
            }
            _ = arp_tick.tick() => {
                let hits = arp_scan().await;
                let unverified: Vec<(String, String)> = hits
                    .iter()
                    .filter(|(ip, _)| !merger.ip_known(ip))
                    .cloned()
                    .collect();
                let alive_flags = futures::future::join_all(
                    unverified.iter().map(|(ip, _)| go2_alive(ip)),
                )
                .await;
                let alive: HashMap<String, bool> = unverified
                    .iter()
                    .map(|(ip, _)| ip.clone())
                    .zip(alive_flags)
                    .collect();
                for (ip, mac) in &hits {
                    if merger.ip_known(ip) {
                        merger.arp_upsert(ip, mac);
                    } else if *alive.get(ip).unwrap_or(&false) {
                        merger.arp_upsert(ip, mac);
                    } else {
                        merger.arp_drop(ip);
                    }
                }
            }
        }
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

    emit(&json!({ "type": "scan_done", "count": merger.merged.len() }));
}
