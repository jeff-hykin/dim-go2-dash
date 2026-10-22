// The first-run guide (the page's Setup): the step the user is on and the robot they picked, saved so a reload, another
// viewer or the agent picks up where they left off; checking that a robot answers at an IP; and calling Desktop's dimos
// gateway (desktop_call, for uploads and the cloud account).

use std::time::Duration;

use serde_json::{json, Value};

use crate::api::HttpError;
use crate::app::{valid_ipv4, App};

pub const STEPS: [&str; 5] = ["welcome", "find", "wifi", "address", "done"];
pub const SETUP_FILE: &str = "go2_dash_setup.json";

/// A fresh setup: the guide opens at its welcome on a first run, and stays closed for someone who already has robots.
pub fn initial(first_run: bool) -> Value {
    json!({ "step": if first_run { "welcome" } else { "done" }, "robot": null })
}

/// A saved guide: its step and robot, an unknown step read as done.
pub fn restore(saved: &Value) -> Value {
    let step = saved["step"].as_str().filter(|step| STEPS.contains(step)).unwrap_or("done");
    json!({ "step": step, "robot": saved["robot"] })
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

    /// Moves the guide: any of its step, the robot picked (a key from the scan; "" forgets it) and that robot's IP (saved
    /// for it too).
    pub fn update_setup(&self, step: Option<&str>, robot: Option<&str>, ip: Option<&str>) -> Result<Value, HttpError> {
        let mut setup = self.setup.lock().unwrap().clone();
        if let Some(step) = step {
            if !STEPS.contains(&step) {
                return Err(HttpError::bad(format!("step must be one of {}", STEPS.join(", "))));
            }
            setup["step"] = json!(step);
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
            return Ok(
                json!({ "ip": ip, "port": port, "reachable": reachable, "ms": if reachable { json!(4) } else { Value::Null }, "error": if reachable { Value::Null } else { json!("no answer (mock)") }, "mock": true }),
            );
        }
        // firmware before 1.1.11 signals on the legacy port instead
        let mut answer = Value::Null;
        for port in [port, crate::robot_rtc::LEGACY_SIGNALING_PORT] {
            let started = std::time::Instant::now();
            let attempt = tokio::time::timeout(Duration::from_millis(2500), tokio::net::TcpStream::connect((ip, port))).await;
            let (reachable, error) = match attempt {
                Ok(Ok(_)) => (true, None),
                Ok(Err(err)) => (false, Some(err.to_string())),
                Err(_) => (false, Some("no answer in 2.5 s".to_string())),
            };
            let this = json!({ "ip": ip, "port": port, "reachable": reachable, "ms": reachable.then(|| started.elapsed().as_millis() as u64), "error": error });
            if reachable {
                return Ok(this);
            }
            if answer.is_null() {
                answer = this;
            }
        }
        Ok(answer)
    }

    // ── Desktop's dimos gateway ──

    fn desktop(&self) -> Result<String, HttpError> {
        self.desktop_url
            .get()
            .map(|url| url.trim_end_matches('/').to_string())
            .ok_or_else(|| HttpError::conflict("Go2 Ctrl isn't running inside dimOS Desktop"))
    }

    pub(crate) async fn desktop_call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, HttpError> {
        let (status, data) = self.desktop_send(method, path, body).await?;
        if status.is_success() {
            return Ok(data);
        }
        let message = data["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("Desktop answered HTTP {status}"));
        Err(HttpError::new(if status.as_u16() == 503 || status.is_server_error() { 502 } else { 409 }, message))
    }

    async fn desktop_send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<(reqwest::StatusCode, Value), HttpError> {
        let url = format!("{}{path}", self.desktop()?);
        let mut request = reqwest::Client::new().request(method, &url).timeout(Duration::from_secs(20));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|err| HttpError::upstream(format!("Desktop didn't answer ({err})")))?;
        let status = response.status();
        Ok((status, response.json().await.unwrap_or(Value::Null)))
    }

    /// What dimos needs to reach a robot, for running it by hand: its IP and AES key, and the line to export them.
    pub fn robot_env(&self, key: &str) -> Result<Value, HttpError> {
        let robot = self.robot(key)?;
        let ip =
            robot["ip"].as_str().filter(|ip| !ip.is_empty()).map(str::to_string).ok_or_else(|| {
                HttpError::conflict(format!("{key} has no IP yet: put it on Wi-Fi, scan again, or PUT api/robots/{key}/ip"))
            })?;
        let aes = self.aes_key_for(key);
        if aes.is_empty() {
            return Err(HttpError::conflict(format!(
                "no AES key saved for {key}: PUT api/robots/{key}/aes-key, or fetch it with POST api/accounts"
            )));
        }
        Ok(json!({
            "ROBOT_IP": ip,
            "UNITREE_AES_128_KEY": aes,
            "export": format!("export ROBOT_IP={ip} UNITREE_AES_128_KEY={aes}"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_a_saved_guide() {
        let old = restore(&json!({ "step": "launch", "mode": "replay", "robot": { "key": "X" } }));
        assert_eq!(old, json!({ "step": "done", "robot": { "key": "X" } }));
        assert_eq!(restore(&json!({ "step": "address", "robot": null }))["step"], "address");
    }
}
