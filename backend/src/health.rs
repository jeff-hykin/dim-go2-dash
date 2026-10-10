// The computer's and the link's side of a connection problem. Every SAMPLE_EVERY a sample (CPU of the machine and this
// app, load, memory, the Wi-Fi: interface, SSID, signal, bitrates, frequency, address, default route; the robot link's
// status and how long since the dog last sent anything; the dog's odom speed) goes into a RAM ring of the last BUFFER.
// Every LOG_EVERY one of them is a "health:" line in go2-ctrl.log (and, while recording with logs on, a
// {"type":"health"} message on /logs). When something looks wrong (see `conditions`) the samples since the last dump go
// into the recording as {"type":"health_dump","reason",…,"samples":[…]} and into the log as "health detail:" lines.
// Linux reads /proc and `iw`/`ip`; macOS asks `sysctl`, `ps`, `ipconfig` and `route`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::app::App;

const SAMPLE_EVERY: Duration = Duration::from_millis(500);
const BUFFER: usize = 240; // 2 minutes
const LOG_EVERY: u64 = 4; // samples: every 2 s

const WEAK_SIGNAL_DBM: f64 = -80.0;
const SILENT_MS: u64 = 1500;
const CRAZY_SPEED: f64 = 4.0; // m/s; a Go2 tops out near 3.7
const CRAZY_YAW: f64 = 6.0; // rad/s
const CPU_PEGGED: f64 = 95.0;

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

// ── what the robot's data channel tells us (robot_rtc.rs calls these) ──

static LAST_RX_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Default, Clone, Copy)]
struct Motion {
    speed: f64,
    yaw_speed: f64,
    position: Option<[f64; 3]>,
    at_ms: u64,
    /// the fastest position change between two sport-state messages since the last sample (m/s)
    implied_speed: f64,
}
static MOTION: Mutex<Motion> =
    Mutex::new(Motion { speed: 0.0, yaw_speed: 0.0, position: None, at_ms: 0, implied_speed: 0.0 });

/// Any message from the dog.
pub fn robot_rx() {
    LAST_RX_MS.store(now_ms(), Ordering::Relaxed);
}

/// rt/lf/sportmodestate's data: velocity [x, y, z], yaw_speed, position [x, y, z].
pub fn robot_motion(data: &Value) {
    let triple = |v: &Value| -> Option<[f64; 3]> {
        let a = v.as_array().filter(|a| a.len() >= 3)?;
        Some([a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?])
    };
    let now = now_ms();
    let mut motion = MOTION.lock().unwrap();
    if let Some([x, y, _]) = triple(&data["velocity"]) {
        motion.speed = x.hypot(y);
    }
    motion.yaw_speed = data["yaw_speed"].as_f64().unwrap_or(motion.yaw_speed);
    if let Some(position) = triple(&data["position"]) {
        if let Some(last) = motion.position {
            let dt = now.saturating_sub(motion.at_ms) as f64 / 1000.0;
            if dt > 0.0 && dt < 0.5 {
                let moved = (position[0] - last[0]).hypot(position[1] - last[1]);
                motion.implied_speed = motion.implied_speed.max(moved / dt.max(0.02));
            }
        }
        motion.position = Some(position);
        motion.at_ms = now;
    }
}

// ── the computer ──

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

#[cfg(any(target_os = "linux", test))]
/// /proc/net/route: the default route's gateway ("wlan0 via 192.168.12.1"), several joined by " | "
fn default_routes(table: &str) -> String {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.get(1)? != &"00000000" {
                return None;
            }
            let gw = u32::from_str_radix(f.get(2)?, 16).ok()?.to_le_bytes();
            Some(format!("{} via {}.{}.{}.{} metric {}", f[0], gw[0], gw[1], gw[2], gw[3], f.get(6)?))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[derive(Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Sampler {
    count: u64,
    last_system: Option<(u64, u64)>,
    last_process: Option<(u64, std::time::Instant)>,
    /// the slower lookups, refreshed every LOG_EVERY samples
    addr: Value,
    ssid: Value,
}

impl Sampler {
    #[cfg(target_os = "linux")]
    async fn sample(&mut self, _app: &App) -> Value {
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
        let kb = |text: String, key: &str| -> Option<u64> {
            text.lines().find_map(|line| line.strip_prefix(key)).and_then(|v| v.split_whitespace().next()?.parse().ok())
        };
        // "wlan0: 0000   54.  -56.  -256 …": the interface, link quality and signal level (dBm), even without `iw`
        let wireless = read("/proc/net/wireless").lines().nth(2).map(|line| line.to_string());
        let iface = wireless.as_deref().and_then(|line| line.split(':').next()).map(|s| s.trim().to_string());
        let wifi = match &iface {
            Some(iface) => {
                let mut wifi = parse_iw_link(&run("iw", &["dev", iface, "link"]).await);
                wifi["iface"] = json!(iface);
                wifi["level"] =
                    json!(wireless.as_deref().and_then(|line| line.split_whitespace().nth(3)).map(|v| v.trim_end_matches('.')));
                if self.count % LOG_EVERY == 0 {
                    self.addr = json!(run("ip", &["-4", "-o", "addr", "show", iface]).await.split_whitespace().nth(3));
                }
                wifi["addr"] = self.addr.clone();
                wifi
            }
            None => json!(null),
        };
        json!({
            "cpu": cpu.flatten().map(|p| (p * 10.0).round() / 10.0),
            "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "load": read("/proc/loadavg").split_whitespace().take(3).collect::<Vec<_>>().join(" "),
            "appCpu": app_cpu.flatten().map(|p| p.round()),
            "appRssMb": kb(read("/proc/self/status"), "VmRSS:").map(|kb| kb / 1024),
            "memAvailableMb": kb(read("/proc/meminfo"), "MemAvailable:").map(|kb| kb / 1024),
            "wifi": wifi,
            "route": default_routes(&read("/proc/net/route")),
        })
    }

    #[cfg(not(target_os = "linux"))]
    async fn sample(&mut self, app: &App) -> Value {
        let pid = std::process::id().to_string();
        let ps = run("ps", &["-o", "%cpu=,rss=", "-p", &pid]).await;
        let mut ps = ps.split_whitespace();
        let route = run("route", &["-n", "get", "default"]).await;
        let field = |key: &str| route.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()));
        let iface = field("interface:");
        if self.count % LOG_EVERY == 0 {
            self.addr = match &iface {
                Some(iface) => json!(run("ipconfig", &["getifaddr", iface]).await),
                None => json!(null),
            };
            self.ssid = json!(app.wifi.current().await);
        }
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        // every process's share of one core, summed, over the cores: the machine's CPU %
        let all: f64 = run("ps", &["-A", "-o", "%cpu="]).await.lines().filter_map(|l| l.trim().parse::<f64>().ok()).sum();
        json!({
            "cpu": (all / cores as f64 * 10.0).round() / 10.0,
            "cores": cores,
            "load": run("sysctl", &["-n", "vm.loadavg"]).await.trim_matches(|c| c == '{' || c == '}' || c == ' ').to_string(),
            "appCpu": ps.next().and_then(|v| v.parse::<f64>().ok()),
            "appRssMb": ps.next().and_then(|v| v.parse::<u64>().ok()).map(|kb| kb / 1024),
            "wifi": { "iface": iface, "addr": self.addr, "ssid": self.ssid },
            "route": field("gateway:").unwrap_or_default(),
        })
    }
}

/// The busiest processes and the memory picture, for when the CPU is pegged.
async fn top_processes() -> Value {
    #[cfg(target_os = "linux")]
    let (ps, mem) = (
        run("ps", &["-eo", "pid,pcpu,pmem,rss,etime,args", "--sort=-pcpu"]).await,
        run("free", &["-m"]).await,
    );
    #[cfg(not(target_os = "linux"))]
    let (ps, mem) = (run("ps", &["-Areo", "pid,pcpu,pmem,rss,etime,command"]).await, run("vm_stat", &[]).await);
    let lines: Vec<String> = ps.lines().take(13).map(|l| l.chars().take(200).collect()).collect();
    json!({ "processes": lines, "memory": mem })
}

// ── what counts as weird ──

fn dbm(text: &Value) -> Option<f64> {
    text.as_str()?.split_whitespace().next()?.parse().ok()
}

/// Everything wrong in one sample, by key (a condition dumps once, when it starts) → what to say about it.
fn conditions(sample: &Value) -> BTreeMap<&'static str, String> {
    let mut wrong = BTreeMap::new();
    let wifi = &sample["wifi"];
    if wifi.is_null() || wifi["connected"] == false {
        wrong.insert("wifi", "Wi-Fi disconnected".to_string());
    }
    if let Some(signal) = dbm(&wifi["signal"]).or_else(|| dbm(&wifi["level"])) {
        if signal < WEAK_SIGNAL_DBM {
            wrong.insert("signal", format!("weak signal {signal} dBm"));
        }
    }
    let robot = &sample["robot"];
    if let Some(status) = robot["status"].as_str() {
        if status != "ready" && status != "connecting" {
            wrong.insert("link", format!("robot link {status}"));
        }
        if status == "ready" && robot["silentMs"].as_u64().is_some_and(|ms| ms > SILENT_MS) {
            wrong.insert("silent", format!("robot silent for {} ms", robot["silentMs"]));
        }
    }
    let fast = |key: &str| sample["odom"][key].as_f64().unwrap_or(0.0);
    if fast("speed") > CRAZY_SPEED || fast("impliedSpeed") > CRAZY_SPEED * 1.5 {
        wrong.insert("speed", format!("odom speed {:.1} m/s (position implies {:.1})", fast("speed"), fast("impliedSpeed")));
    }
    if fast("yawSpeed").abs() > CRAZY_YAW {
        wrong.insert("yaw", format!("odom yaw speed {:.1} rad/s", fast("yawSpeed")));
    }
    if sample["cpu"].as_f64().is_some_and(|cpu| cpu > CPU_PEGGED) {
        wrong.insert("cpu", format!("CPU {}%", sample["cpu"]));
    }
    wrong
}

/// What a "wifi changed" dump is about: the network, the address and the route, not the signal's wobble.
fn link_key(sample: &Value) -> String {
    format!("{} {} {}", sample["wifi"]["ssid"], sample["wifi"]["addr"], sample["route"])
}

pub fn spawn(app: Arc<App>) {
    tokio::spawn(async move {
        let mut sampler = Sampler::default();
        let mut ring: VecDeque<Value> = VecDeque::with_capacity(BUFFER);
        let mut last_key = String::new();
        let mut active: BTreeMap<&'static str, String> = BTreeMap::new();
        // samples after this time haven't been dumped yet
        let mut dumped_to_ms = 0u64;
        let mut top_at_ms = 0u64;
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut sample = sampler.sample(&app).await;
            sampler.count += 1;
            let t = now_ms();
            sample["t"] = json!(t);
            let drive = app.drive.lock().await.clone();
            if let Some(drive) = drive.filter(|d| !d.dry) {
                let last_rx = LAST_RX_MS.load(Ordering::Relaxed);
                sample["robot"] = json!({
                    "status": drive.snapshot()["status"],
                    "silentMs": (last_rx > 0).then(|| t.saturating_sub(last_rx)),
                });
                let motion = std::mem::replace(&mut MOTION.lock().unwrap().implied_speed, 0.0);
                let m = *MOTION.lock().unwrap();
                sample["odom"] = json!({
                    "speed": (m.speed * 100.0).round() / 100.0,
                    "yawSpeed": (m.yaw_speed * 100.0).round() / 100.0,
                    "impliedSpeed": (motion * 100.0).round() / 100.0,
                    "position": m.position,
                });
            }
            if ring.len() == BUFFER {
                ring.pop_front();
            }
            ring.push_back(sample.clone());

            let mut reasons: Vec<String> = Vec::new();
            let key = link_key(&sample);
            if !last_key.is_empty() && key != last_key {
                reasons.push(format!("wifi changed: {}", sample["wifi"]));
            }
            last_key = key;
            let now_wrong = conditions(&sample);
            for (condition, text) in &now_wrong {
                if !active.contains_key(condition) {
                    reasons.push(text.clone());
                }
            }
            active = now_wrong;

            let recorder = app.recording().filter(|r| r.recorder.logs_on());
            // CPU pegged: what's eating it, when it starts and every 10 s while it lasts
            let top = if active.contains_key("cpu") && t.saturating_sub(top_at_ms) >= 10_000 {
                top_at_ms = t;
                let top = top_processes().await;
                crate::dlog!("health top: {top}");
                if let Some(active) = recorder.as_ref().filter(|_| reasons.is_empty()) {
                    let event = json!({ "type": "health_top", "t": t, "top": top });
                    active.recorder.write("/logs", crate::msg::Msg::Text(event.to_string()));
                }
                Some(top)
            } else {
                None
            };
            if !reasons.is_empty() {
                let reason = reasons.join("; ");
                let samples: Vec<&Value> = ring.iter().filter(|s| s["t"].as_u64().unwrap_or(0) > dumped_to_ms).collect();
                crate::dlog!("health event: {reason} ({} samples follow)", samples.len());
                for s in &samples {
                    crate::dlog!("health detail: {s}");
                }
                if let Some(active) = &recorder {
                    let dump = json!({ "type": "health_dump", "reason": reason, "top": top, "samples": samples });
                    active.recorder.write("/logs", crate::msg::Msg::Text(dump.to_string()));
                }
                dumped_to_ms = t;
            } else if sampler.count % LOG_EVERY == 0 {
                crate::dlog!("health: {sample}");
                if let Some(active) = &recorder {
                    sample["type"] = json!("health");
                    active.recorder.write("/logs", crate::msg::Msg::Text(sample.to_string()));
                }
            }
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
    fn reads_iw_link_and_routes() {
        let text = "Connected to 11:22:33:44:55:66 (on wlan0)\n\tSSID: Go2_60968_83d1a1fa\n\tfreq: 5745.0\n\tsignal: -48 dBm\n\trx bitrate: 433.3 MBit/s\n\ttx bitrate: 390.0 MBit/s VHT-MCS 8";
        let link = parse_iw_link(text);
        assert_eq!(link["ssid"], "Go2_60968_83d1a1fa");
        assert_eq!(link["signal"], "-48 dBm");
        assert_eq!(link["txBitrate"], "390.0 MBit/s VHT-MCS 8");
        assert_eq!(parse_iw_link("Not connected.")["connected"], false);
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\nwlan0\t00000000\t010CA8C0\t0003\t0\t0\t20000\t00000000\nwlan0\t000CA8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF";
        assert_eq!(default_routes(table), "wlan0 via 192.168.12.1 metric 20000");
    }

    #[test]
    fn flags_what_looks_wrong() {
        let calm = json!({ "cpu": 30.0, "wifi": { "signal": "-50 dBm", "connected": null },
            "robot": { "status": "ready", "silentMs": 40 }, "odom": { "speed": 1.2, "yawSpeed": 0.5, "impliedSpeed": 1.3 } });
        assert!(conditions(&calm).is_empty());
        let bad = json!({ "cpu": 99.0, "wifi": { "signal": "-86 dBm" },
            "robot": { "status": "ready", "silentMs": 2500 }, "odom": { "speed": 0.1, "yawSpeed": 9.0, "impliedSpeed": 40.0 } });
        let keys: Vec<_> = conditions(&bad).into_keys().collect();
        assert_eq!(keys, ["cpu", "signal", "silent", "speed", "yaw"]);
        assert!(conditions(&json!({ "wifi": { "connected": false } })).contains_key("wifi"));
        assert!(conditions(&json!({ "wifi": {}, "robot": { "status": "reconnecting" } })).contains_key("link"));
    }
}
