// The first-run guide (the page's Setup): the step the user is on and the robot they picked, saved so a reload, another
// viewer or the agent picks up where they left off; checking that a robot answers at an IP; and launching dimos for it
// through Desktop (POST <desktopUrl>/dimos/runs with robot_ip, plus its AES key when one is saved), or a replay.

use std::time::Duration;

use serde_json::{json, Value};

use crate::api::HttpError;
use crate::app::{now_ms, valid_ipv4, App};

pub const STEPS: [&str; 6] = ["welcome", "find", "wifi", "address", "launch", "done"];
/// what the Launcher puts first for a Go2: camera, lidar and pose, driven from Controller
pub const BLUEPRINT: &str = "unitree-go2-basic";
pub const SETUP_FILE: &str = "go2_dash_setup.json";
const AES_FLAG: &str = "unitree_aes_128_key";
/// a mock launch takes this long per startup step
const MOCK_STEP_MS: u64 = 1200;

/// A fresh setup: the guide opens at its welcome on a first run, and stays closed for someone who already has robots.
pub fn initial(first_run: bool) -> Value {
    json!({ "step": if first_run { "welcome" } else { "done" }, "mode": null, "robot": null })
}

fn valid_blueprint(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Desktop's Launch, for a page: its AES key hidden, its output cut to the last lines.
pub fn sanitize_launch(mut launch: Value) -> Value {
    if launch.is_null() {
        return launch;
    }
    if let Some(key) = launch["overrides"].get_mut(AES_FLAG) {
        *key = json!("(saved key)");
    }
    if let Some(output) = launch["output"].as_str() {
        let lines: Vec<&str> = output.lines().collect();
        let tail = lines[lines.len().saturating_sub(12)..]
            .iter()
            .map(|line| redact_flag(line))
            .collect::<Vec<_>>()
            .join("\n");
        launch["output"] = json!(tail);
    }
    launch
}

/// `--unitree-aes-128-key <hex>` (or `_` spelled) in a command line → the value hidden.
fn redact_flag(line: &str) -> String {
    let words: Vec<&str> = line.split(' ').collect();
    let mut out = Vec::with_capacity(words.len());
    let mut hide = false;
    for word in words {
        if hide {
            out.push("(saved key)");
            hide = false;
            continue;
        }
        hide = word.trim_start_matches('-').replace('-', "_") == AES_FLAG;
        out.push(word);
    }
    out.join(" ")
}

/// Startup steps of a simulated launch, shaped like Desktop's (diagnose.rs), at `elapsed` ms.
pub fn mock_launch_view(blueprint: &str, overrides: &Value, started: u64, elapsed: u64) -> Value {
    let labels = ["Starting dimOS", "Building the blueprint", "Starting modules", "Running"];
    let reached = (elapsed / MOCK_STEP_MS) as usize;
    let running = reached >= labels.len() - 1;
    let steps: Vec<Value> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let state = if running || i < reached {
                "done"
            } else if i == reached {
                "now"
            } else {
                "todo"
            };
            json!({ "label": label, "state": state, "detail": null })
        })
        .collect();
    json!({
        "blueprint": blueprint,
        "phase": if running { "running" } else { "starting" },
        "startedAt": started,
        "pid": 0,
        "output": format!("$ dimos --robot-ip {} run {blueprint}\n(mock: simulated, nothing started)", overrides["robot_ip"].as_str().unwrap_or("?")),
        "runId": if running { json!("mock") } else { Value::Null },
        "error": null,
        "overrides": overrides,
        "steps": steps,
        "problems": [],
        "mock": true,
    })
}

impl App {
    // ── the guide's step and robot ──

    pub fn setup_state(&self) -> Value {
        let mut setup = self.setup.lock().unwrap().clone();
        if let Some(key) = setup["robot"]["key"].as_str().map(str::to_string) {
            setup["robot"]["hasAesKey"] = json!(!self.aes_key_for(&key).is_empty());
        }
        setup
    }

    fn store_setup(&self, setup: Value) -> Value {
        *self.setup.lock().unwrap() = setup;
        let setup = self.setup_state();
        let mut saved = setup.clone();
        if let Some(robot) = saved["robot"].as_object_mut() {
            robot.remove("hasAesKey");
        }
        self.save_file(SETUP_FILE, saved);
        self.publish(json!({ "type": "setup", "setup": setup }));
        setup
    }

    /// Moves the guide: any of its step, the robot picked (a key from the scan; "" forgets it), that robot's IP (saved
    /// for it too), and the mode ("robot", or "replay" for someone without one).
    pub fn update_setup(&self, step: Option<&str>, robot: Option<&str>, ip: Option<&str>, mode: Option<&str>) -> Result<Value, HttpError> {
        let mut setup = self.setup.lock().unwrap().clone();
        if let Some(step) = step {
            if !STEPS.contains(&step) {
                return Err(HttpError::bad(format!("step must be one of {}", STEPS.join(", "))));
            }
            setup["step"] = json!(step);
        }
        if let Some(mode) = mode {
            setup["mode"] = match mode {
                "robot" | "replay" => json!(mode),
                "" => Value::Null,
                _ => return Err(HttpError::bad("mode must be robot, replay or empty")),
            };
        }
        if let Some(key) = robot {
            setup["robot"] = if key.is_empty() {
                Value::Null
            } else {
                let found = self.robot(key)?;
                json!({ "key": found["key"], "name": found["name"], "ip": found["ip"], "serial": found["serial"], "bleId": found["bleId"] })
            };
        }
        if let Some(ip) = ip.map(str::trim) {
            if !ip.is_empty() && !valid_ipv4(ip) {
                return Err(HttpError::bad(format!("{ip} isn't an IPv4 address")));
            }
            if setup["robot"].is_null() {
                if ip.is_empty() {
                    return Err(HttpError::bad("pick a robot (robot) or give its IP"));
                }
                // no scan found it: the robot is the IP someone typed
                setup["robot"] = json!({ "key": ip, "name": format!("Go2 at {ip}"), "ip": ip, "serial": null, "bleId": null });
            } else {
                setup["robot"]["ip"] = if ip.is_empty() { Value::Null } else { json!(ip) };
                let key = setup["robot"]["key"].as_str().unwrap_or("").to_string();
                if self.robot(&key).is_ok() {
                    self.remember_ip_public(&key, ip);
                }
            }
        }
        Ok(self.store_setup(setup))
    }

    pub fn reset_setup(&self) -> Value {
        self.store_setup(initial(true))
    }

    // ── does a Go2 answer there ──

    /// Opens (and closes) a TCP connection to the Go2's WebRTC signaling port; sends nothing.
    pub async fn check_ip(&self, ip: &str) -> Result<Value, HttpError> {
        let ip = ip.trim();
        if !valid_ipv4(ip) {
            return Err(HttpError::bad(format!("{ip} isn't an IPv4 address")));
        }
        let port = crate::robot_rtc::SIGNALING_PORT;
        if self.mock {
            tokio::time::sleep(Duration::from_millis(600)).await;
            // the mock's robots live in TEST-NET-1; anything else is "not there"
            let reachable = ip.starts_with("192.0.2.");
            return Ok(json!({ "ip": ip, "port": port, "reachable": reachable, "ms": if reachable { json!(4) } else { Value::Null }, "error": if reachable { Value::Null } else { json!("no answer (mock)") }, "mock": true }));
        }
        let started = std::time::Instant::now();
        let attempt = tokio::time::timeout(Duration::from_millis(2500), tokio::net::TcpStream::connect((ip, port))).await;
        let (reachable, error) = match attempt {
            Ok(Ok(_)) => (true, None),
            Ok(Err(err)) => (false, Some(err.to_string())),
            Err(_) => (false, Some("no answer in 2.5 s".to_string())),
        };
        Ok(json!({ "ip": ip, "port": port, "reachable": reachable, "ms": reachable.then(|| started.elapsed().as_millis() as u64), "error": error }))
    }

    // ── launching dimos through Desktop ──

    fn desktop(&self) -> Result<String, HttpError> {
        self.desktop_url
            .get()
            .map(|url| url.trim_end_matches('/').to_string())
            .ok_or_else(|| HttpError::conflict("Go2 Ctrl isn't running inside dimOS Desktop, so it can't launch dimos"))
    }

    async fn desktop_call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, HttpError> {
        let url = format!("{}{path}", self.desktop()?);
        let mut request = reqwest::Client::new().request(method, &url).timeout(Duration::from_secs(20));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|err| HttpError::upstream(format!("Desktop didn't answer ({err})")))?;
        let status = response.status();
        let data: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(data);
        }
        let message = data["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("Desktop answered HTTP {status}"));
        Err(HttpError::new(if status.as_u16() == 503 || status.is_server_error() { 502 } else { 409 }, message))
    }

    /// What POST /dimos/runs gets: the blueprint, replay, and robot_ip (+ the robot's AES key) for a real robot.
    pub fn launch_request(&self, replay: bool, blueprint: Option<&str>, ip: Option<&str>) -> Result<Value, HttpError> {
        let blueprint = blueprint.filter(|b| !b.is_empty()).unwrap_or(BLUEPRINT);
        if !valid_blueprint(blueprint) {
            return Err(HttpError::bad(format!("{blueprint} isn't a blueprint name")));
        }
        if replay {
            return Ok(json!({ "blueprint": blueprint, "replay": true, "overrides": {} }));
        }
        let setup = self.setup_state();
        let ip = ip.filter(|ip| !ip.trim().is_empty()).map(|ip| ip.trim().to_string()).or_else(|| setup["robot"]["ip"].as_str().map(str::to_string)).ok_or_else(|| {
            HttpError::bad("no robot IP: give ip, or pick a robot with an IP first (PUT api/setup)")
        })?;
        if !valid_ipv4(&ip) {
            return Err(HttpError::bad(format!("{ip} isn't an IPv4 address")));
        }
        let mut overrides = json!({ "robot_ip": ip });
        // the saved key of the robot at that IP (the guide's, else a scanned one)
        let key = setup["robot"]["key"]
            .as_str()
            .filter(|_| setup["robot"]["ip"].as_str() == Some(ip.as_str()))
            .map(str::to_string)
            .or_else(|| self.robots().into_iter().find(|r| r["ip"] == ip.as_str()).and_then(|r| r["key"].as_str().map(str::to_string)));
        if let Some(aes) = key.map(|k| self.aes_key_for(&k)).filter(|k| !k.is_empty()) {
            overrides[AES_FLAG] = json!(aes);
        }
        Ok(json!({ "blueprint": blueprint, "replay": false, "overrides": overrides }))
    }

    /// Launches dimos (Desktop runs one launch at a time: a running one answers 409 with what to stop). In mock mode a
    /// robot launch is simulated; a replay really starts. `as_default` also saves robot_ip in Desktop's global config,
    /// so the Launcher's launches reach this robot too.
    pub async fn launch(&self, replay: bool, blueprint: Option<&str>, ip: Option<&str>, dry_run: bool, as_default: bool) -> Result<Value, HttpError> {
        let request = self.launch_request(replay, blueprint, ip)?;
        let mut shown = request.clone();
        if shown["overrides"].get(AES_FLAG).is_some() {
            shown["overrides"][AES_FLAG] = json!("(saved key)");
        }
        if dry_run {
            return Ok(json!({ "dryRun": true, "sent": false, "request": shown, "default": as_default && !replay }));
        }
        if self.mock && !replay {
            let started = now_ms();
            let overrides = shown["overrides"].clone();
            *self.mock_launch.lock().unwrap() = Some((request["blueprint"].as_str().unwrap_or(BLUEPRINT).to_string(), overrides, started));
            let launch = self.launch_state().await?;
            self.publish(json!({ "type": "launch", "launch": launch }));
            return Ok(launch);
        }
        *self.mock_launch.lock().unwrap() = None;
        if as_default && !replay {
            self.save_default_ip(request["overrides"]["robot_ip"].as_str().unwrap_or("")).await?;
        }
        let launch = sanitize_launch(self.desktop_call(reqwest::Method::POST, "/dimos/runs", Some(request)).await?);
        self.publish(json!({ "type": "launch", "launch": launch }));
        Ok(launch)
    }

    async fn save_default_ip(&self, ip: &str) -> Result<(), HttpError> {
        let current = self.desktop_call(reqwest::Method::GET, "/dimos/global-config", None).await?;
        let mut overrides = current["overrides"].as_object().cloned().unwrap_or_default();
        overrides.insert("robot_ip".into(), json!(ip));
        self.desktop_call(reqwest::Method::PUT, "/dimos/global-config", Some(json!({ "overrides": overrides }))).await?;
        Ok(())
    }

    /// Desktop's launch (the mock's simulated one in mock mode), or null when there is none.
    pub async fn launch_state(&self) -> Result<Value, HttpError> {
        let mock = self.mock_launch.lock().unwrap().clone();
        if let Some((blueprint, overrides, started)) = mock {
            return Ok(mock_launch_view(&blueprint, &overrides, started, now_ms().saturating_sub(started)));
        }
        if self.mock && self.desktop_url.get().is_none() {
            return Ok(Value::Null);
        }
        Ok(sanitize_launch(self.desktop_call(reqwest::Method::GET, "/dimos/runs", None).await?["launch"].take()))
    }

    pub async fn stop_launch(&self) -> Result<Value, HttpError> {
        if self.mock_launch.lock().unwrap().take().is_some() {
            self.publish(json!({ "type": "launch", "launch": null }));
            return Ok(json!({ "stopped": true, "mock": true }));
        }
        let out = self.desktop_call(reqwest::Method::POST, "/dimos/runs/stop", Some(json!({}))).await?;
        let launch = self.launch_state().await.unwrap_or(Value::Null);
        self.publish(json!({ "type": "launch", "launch": launch }));
        Ok(json!({ "stopped": true, "output": out["output"] }))
    }
}

/// a simulated robot launch: blueprint, overrides (key hidden), when it started
pub type MockLaunch = std::sync::Mutex<Option<(String, Value, u64)>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_the_aes_key() {
        let launch = json!({ "overrides": { "robot_ip": "10.0.0.5", "unitree_aes_128_key": "00112233445566778899aabbccddeeff" }, "output": "$ dimos --robot-ip 10.0.0.5 --unitree-aes-128-key 00112233445566778899aabbccddeeff run unitree-go2-basic\nStarting DimOS" });
        let shown = sanitize_launch(launch);
        assert_eq!(shown["overrides"]["unitree_aes_128_key"], "(saved key)");
        assert!(!shown["output"].as_str().unwrap().contains("0011"), "{}", shown["output"]);
        assert!(shown["output"].as_str().unwrap().contains("--robot-ip 10.0.0.5"));
        assert_eq!(redact_flag("dimos --unitree_aes_128_key abc run x"), "dimos --unitree_aes_128_key (saved key) run x");
    }

    #[test]
    fn mock_launch_walks_the_steps() {
        let overrides = json!({ "robot_ip": "192.0.2.10" });
        let first = mock_launch_view("unitree-go2-basic", &overrides, 0, 0);
        assert_eq!(first["phase"], "starting");
        assert_eq!(first["steps"][0]["state"], "now");
        let later = mock_launch_view("unitree-go2-basic", &overrides, 0, MOCK_STEP_MS * 2 + 10);
        assert_eq!(later["steps"][1]["state"], "done");
        assert_eq!(later["steps"][2]["state"], "now");
        let done = mock_launch_view("unitree-go2-basic", &overrides, 0, MOCK_STEP_MS * 3);
        assert_eq!(done["phase"], "running");
        assert!(done["steps"].as_array().unwrap().iter().all(|s| s["state"] == "done"));
    }

    #[test]
    fn blueprint_names() {
        assert!(valid_blueprint("unitree-go2-basic"));
        assert!(!valid_blueprint("--help"));
        assert!(!valid_blueprint("a b"));
        assert!(!valid_blueprint(""));
    }
}
