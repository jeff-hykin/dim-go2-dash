// The computer's side of a connection problem: every HEALTH_EVERY a line in go2-ctrl.log (and, while recording with logs
// on, a {"type":"health"} message on /logs) with the CPU (whole machine and this app), load, memory, and the Wi-Fi
// (interface, SSID, signal, bitrate, frequency, address, default route); a "wifi changed" line as soon as the network,
// address or route changes. Linux reads /proc and `iw`/`ip`; macOS asks `sysctl`, `ps`, `ipconfig` and `route`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::app::App;

const HEALTH_EVERY: Duration = Duration::from_secs(5);

async fn run(cmd: &str, args: &[&str]) -> String {
    match tokio::process::Command::new(cmd).args(args).output().await {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(any(target_os = "linux", test))]
/// /proc/stat's first line: (busy, total) jiffies
fn system_ticks(stat: &str) -> Option<(u64, u64)> {
    let fields: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3)? + fields.get(4).unwrap_or(&0);
    Some((total - idle, total))
}

#[cfg(any(target_os = "linux", test))]
/// /proc/self/stat: this process's utime + stime (jiffies); the command name may hold spaces, so count after ')'
fn process_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
}

#[cfg(any(target_os = "linux", test))]
/// `iw dev <if> link`: SSID, signal, tx bitrate, frequency
fn parse_iw_link(text: &str) -> Value {
    let field = |key: &str| text.lines().find_map(|line| line.trim().strip_prefix(key).map(|v| v.trim().to_string()));
    if text.starts_with("Not connected") {
        return json!({ "connected": false });
    }
    json!({
        "ssid": field("SSID:"),
        "signal": field("signal:"),
        "txBitrate": field("tx bitrate:"),
        "rxBitrate": field("rx bitrate:"),
        "freq": field("freq:"),
    })
}

#[derive(Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Sampler {
    last_system: Option<(u64, u64)>,
    last_process: Option<(u64, std::time::Instant)>,
}

impl Sampler {
    #[cfg(target_os = "linux")]
    async fn sample(&mut self) -> Value {
        let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
        let cpu = system_ticks(&read("/proc/stat")).map(|now| {
            let percent = self.last_system.map(|(busy, total)| 100.0 * (now.0 - busy) as f64 / (now.1 - total).max(1) as f64);
            self.last_system = Some(now);
            percent
        });
        // jiffies are 1/100 s on every Linux this runs on (USER_HZ)
        let app_cpu = process_ticks(&read("/proc/self/stat")).map(|ticks| {
            let now = std::time::Instant::now();
            let percent = self.last_process.map(|(last, at)| (ticks - last) as f64 / (now - at).as_secs_f64().max(0.001));
            self.last_process = Some((ticks, now));
            percent
        });
        let rss_kb: Option<u64> = read("/proc/self/status")
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok());
        let mem_available_kb: Option<u64> = read("/proc/meminfo")
            .lines()
            .find_map(|line| line.strip_prefix("MemAvailable:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok());
        // "wlan0: 0000   54.  -56.  -256 …": the interface, link quality and signal level (dBm), even without `iw`
        let wireless = read("/proc/net/wireless").lines().nth(2).map(|line| line.to_string());
        let iface = wireless.as_deref().and_then(|line| line.split(':').next()).map(|s| s.trim().to_string());
        let wifi = match &iface {
            Some(iface) => {
                let mut wifi = parse_iw_link(&run("iw", &["dev", iface, "link"]).await);
                wifi["iface"] = json!(iface);
                wifi["level"] = json!(wireless.as_deref().and_then(|line| line.split_whitespace().nth(3)).map(|v| v.trim_end_matches('.')));
                wifi["addr"] = json!(run("ip", &["-4", "-o", "addr", "show", iface]).await.split_whitespace().nth(3));
                wifi
            }
            None => json!(null),
        };
        json!({
            "cpu": cpu.flatten().map(|p| (p * 10.0).round() / 10.0),
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "load": read("/proc/loadavg").split_whitespace().take(3).collect::<Vec<_>>().join(" "),
            "appCpu": app_cpu.flatten().map(|p| p.round()),
            "appRssMb": rss_kb.map(|kb| kb / 1024),
            "memAvailableMb": mem_available_kb.map(|kb| kb / 1024),
            "wifi": wifi,
            "route": run("ip", &["route", "show", "default"]).await.replace('\n', " | "),
        })
    }

    #[cfg(not(target_os = "linux"))]
    async fn sample(&mut self) -> Value {
        let pid = std::process::id().to_string();
        let ps = run("ps", &["-o", "%cpu=,rss=", "-p", &pid]).await;
        let mut ps = ps.split_whitespace();
        let route = run("route", &["-n", "get", "default"]).await;
        let field = |key: &str| route.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()));
        let iface = field("interface:");
        let addr = match &iface {
            Some(iface) => run("ipconfig", &["getifaddr", iface]).await,
            None => String::new(),
        };
        json!({
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "load": run("sysctl", &["-n", "vm.loadavg"]).await.trim_matches(|c| c == '{' || c == '}' || c == ' ').to_string(),
            "appCpu": ps.next().and_then(|v| v.parse::<f64>().ok()),
            "appRssMb": ps.next().and_then(|v| v.parse::<u64>().ok()).map(|kb| kb / 1024),
            "wifi": { "iface": iface, "addr": addr },
            "route": field("gateway:").unwrap_or_default(),
        })
    }
}

/// What a "wifi changed" line is about: the network, the address and the route, not the signal's wobble.
fn link_key(sample: &Value) -> String {
    format!("{} {} {} {}", sample["wifi"]["ssid"], sample["ssid"], sample["wifi"]["addr"], sample["route"])
}

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut sampler = Sampler::default();
        let mut last_key = String::new();
        loop {
            let mut sample = sampler.sample().await;
            // macOS's SSID (Linux has it from iw): the hotspot switcher knows it
            if sample["wifi"]["ssid"].is_null() {
                sample["ssid"] = json!(app.wifi.current().await);
            }
            let key = link_key(&sample);
            if key != last_key {
                if !last_key.is_empty() {
                    crate::dlog!("wifi changed: {}", sample["wifi"]);
                }
                last_key = key;
            }
            crate::dlog!("health: {sample}");
            if let Some(active) = app.recording() {
                if active.recorder.logs_on() {
                    sample["type"] = json!("health");
                    active.recorder.write("/logs", crate::cdr::string(&sample.to_string()), None);
                }
            }
            tokio::time::sleep(HEALTH_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proc_stat_and_self_stat() {
        let (busy, total) = system_ticks("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3").unwrap();
        assert_eq!((busy, total), (150, 1000));
        let stat = "1234 (dimos app (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 70 30 0 0 20 0 9 0 1 2 3";
        assert_eq!(process_ticks(stat), Some(100));
    }

    #[test]
    fn reads_iw_link() {
        let text = "Connected to 11:22:33:44:55:66 (on wlan0)\n\tSSID: Go2_60968_83d1a1fa\n\tfreq: 5745.0\n\tsignal: -48 dBm\n\trx bitrate: 433.3 MBit/s\n\ttx bitrate: 390.0 MBit/s VHT-MCS 8";
        let link = parse_iw_link(text);
        assert_eq!(link["ssid"], "Go2_60968_83d1a1fa");
        assert_eq!(link["signal"], "-48 dBm");
        assert_eq!(link["txBitrate"], "390.0 MBit/s VHT-MCS 8");
        assert_eq!(parse_iw_link("Not connected.")["connected"], false);
    }
}
