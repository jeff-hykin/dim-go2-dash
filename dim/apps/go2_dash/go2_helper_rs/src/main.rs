// go2_helper — Rust replacement for the go2_dash python helper.
//
// JSON-over-stdio bridge for the dim-go2-dash dashboard backend. Discovers
// Unitree Go2 robots (BLE + LAN + ARP) and provisions their wifi over BLE.
// Standalone: no dimos venv, cross-platform (CoreBluetooth / BlueZ).
//
// Protocol (newline-delimited JSON both ways):
//   stdin  : {"type":"scan","timeout":7}
//            {"type":"connect","mac":..,"ssid":..,"password":..,"country":"US"}
//            {"type":"cancel"}
//   stdout : {"type":"ready"} / {"type":"scan_start"} / {"type":"scan_done","count":n}
//            {"type":"device", serial, name, ble_mac, ip, lan_mac, arp_only}
//            {"type":"drop","key":..} / {"type":"progress","msg":..}
//            {"type":"connect_result","ok":bool,"serial":..,"error":..,"cancelled":bool} / {"type":"warn","msg":..}

mod arp;
mod ble;
mod discovery;
mod lan;
mod protocol;

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use ble::{first_adapter, provision_wifi, Registry};
use btleplug::platform::Adapter;

/// Write one newline-delimited JSON event to stdout and flush (the backend reads
/// line by line and must see events promptly).
pub fn emit(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

#[tokio::main]
async fn main() {
    emit(&json!({ "type": "ready" }));

    let adapter: Option<Adapter> = match first_adapter().await {
        Ok(adapter) => Some(adapter),
        Err(err) => {
            emit(&json!({ "type": "warn", "msg": format!("bluetooth: {err}") }));
            None
        }
    };
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));

    let mut scan_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut connect_task: Option<tokio::task::JoinHandle<()>> = None;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cmd: Value = match serde_json::from_str(line) {
            Ok(cmd) => cmd,
            Err(_) => continue,
        };
        match cmd.get("type").and_then(|v| v.as_str()) {
            Some("scan") => {
                if scan_task.as_ref().is_some_and(|task| !task.is_finished()) {
                    continue; // a scan is already running — ignore re-trigger
                }
                let timeout = cmd.get("timeout").and_then(|v| v.as_f64()).unwrap_or(7.0);
                scan_task = Some(tokio::spawn(discovery::do_scan(
                    adapter.clone(),
                    registry.clone(),
                    timeout,
                )));
            }
            Some("connect") => {
                // A new connect supersedes any in-flight one.
                if let Some(task) = connect_task.take() {
                    task.abort();
                }
                connect_task = Some(tokio::spawn(do_connect(registry.clone(), cmd)));
            }
            Some("cancel") => {
                if let Some(task) = connect_task.take() {
                    if !task.is_finished() {
                        task.abort();
                        emit(&json!({ "type": "connect_result", "ok": false, "cancelled": true }));
                    }
                }
            }
            _ => {}
        }
    }
}

async fn do_connect(registry: Registry, cmd: Value) {
    let mac = cmd.get("mac").and_then(|v| v.as_str()).unwrap_or_default();
    let ssid = cmd.get("ssid").and_then(|v| v.as_str()).unwrap_or_default();
    let password = cmd.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let country = cmd.get("country").and_then(|v| v.as_str()).unwrap_or("US");
    let retries = cmd.get("retries").and_then(|v| v.as_u64()).unwrap_or(3) as u32;

    if mac.is_empty() || ssid.is_empty() {
        emit(&json!({
            "type": "connect_result",
            "ok": false,
            "error": "mac and ssid are required",
        }));
        return;
    }

    emit(&json!({ "type": "progress", "msg": format!("Connecting {mac} → “{ssid}” …") }));

    let peripheral = registry.lock().await.get(mac).cloned();
    let peripheral = match peripheral {
        Some(peripheral) => peripheral,
        None => {
            emit(&json!({
                "type": "connect_result",
                "ok": false,
                "error": format!("device {mac} not found — run a scan first"),
            }));
            return;
        }
    };

    let mut last_error = String::from("provisioning failed");
    for attempt in 0..retries {
        let result = provision_wifi(
            peripheral.clone(),
            ssid,
            password,
            country,
            3,
            |msg| emit(&json!({ "type": "progress", "msg": msg })),
        )
        .await;
        match result {
            Ok(serial) => {
                emit(&json!({ "type": "connect_result", "ok": true, "serial": serial }));
                return;
            }
            Err(err) => {
                last_error = err;
                emit(&json!({
                    "type": "progress",
                    "msg": format!("attempt {} failed: {}", attempt + 1, last_error),
                }));
                if attempt + 1 < retries {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }
    emit(&json!({ "type": "connect_result", "ok": false, "error": last_error }));
}
