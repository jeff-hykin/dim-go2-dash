// The app's scan from a terminal (find-go2.sh, ported): Bluetooth + LAN discovery + the ARP sweep, printed as a table.
// Read-only: it pings and reads the neighbour table; it never connects to a robot.
//   cargo run --example find_go2 -- [quick|full|known|off] [timeout seconds] [remembered ip …]

use std::sync::{Arc, Mutex};

use go2_dash::discovery::{do_scan, ScanOptions, Sink, Stop, SweepMode};
use serde_json::Value;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = SweepMode::parse(args.first().map(String::as_str).unwrap_or("")).expect("sweep: quick, full, known or off");
    let timeout_secs: f64 = args.get(1).and_then(|t| t.parse().ok()).unwrap_or(7.0);
    let known = args.iter().skip(2).filter_map(|ip| ip.parse().ok()).collect();

    let adapter = match go2_dash::ble::first_adapter().await {
        Ok(adapter) => Some(adapter),
        Err(err) => {
            eprintln!("!! bluetooth: {err} (network only)");
            None
        }
    };
    let devices: Arc<Mutex<Vec<Value>>> = Default::default();
    let sink: Sink = {
        let devices = devices.clone();
        let last_progress = Mutex::new(0usize);
        Arc::new(move |event: Value| match event["type"].as_str().unwrap_or("") {
            "device" => {
                let mut devices = devices.lock().unwrap();
                devices.retain(|d| {
                    !(same(d, &event, "serial") || same(d, &event, "ble_mac") || (d["arp_only"] == true && same(d, &event, "ip")))
                });
                devices.push(event);
            }
            "drop" => devices.lock().unwrap().retain(|d| d["ip"] != event["key"] && d["ble_mac"] != event["key"]),
            "sweep" if event["status"] == "running" => {
                let sent = event["sent"].as_u64().unwrap_or(0) as usize;
                let mut last = last_progress.lock().unwrap();
                if sent >= *last + 2000 || sent < *last || event["sent"] == event["total"] {
                    *last = sent;
                    eprintln!(
                        "   sweep {} on {}: {}/{} ({} answered)",
                        event["phase"], event["iface"], sent, event["total"], event["alive"]
                    );
                }
            }
            "sweep" | "warn" | "seen" => eprintln!("   {event}"),
            _ => {}
        })
    };
    let started = std::time::Instant::now();
    let options = ScanOptions { timeout_secs, sweep: mode, known, widen: true, stop: Arc::new(Stop::default()) };
    do_scan(adapter, Default::default(), options, sink).await;

    println!(
        "\nscan took {:.1}s\n{:<14} {:<15} {:<19} {:<19} {:<18} {}",
        started.elapsed().as_secs_f64(),
        "NAME",
        "IP",
        "WIFI-MAC",
        "BLE-MAC",
        "SERIAL",
        "MATCHED"
    );
    for d in devices.lock().unwrap().iter() {
        let get = |k: &str| d[k].as_str().unwrap_or("-").to_string();
        println!(
            "{:<14} {:<15} {:<19} {:<19} {:<18} {}",
            get("name"),
            get("ip"),
            get("lan_mac"),
            get("ble_hw_mac"),
            get("serial"),
            get("matched")
        );
    }
}

fn same(a: &Value, b: &Value, key: &str) -> bool {
    a[key].is_string() && a[key] == b[key]
}
