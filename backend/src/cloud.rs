// Unitree cloud client: the same calls the official Go2 app makes, used to fetch each bound robot's per-device AES-128
// key (the data2=3 WebRTC handshake on Go2 firmware ≥ 1.1.15 / G1 ≥ 1.5.1). Mirrors unitree_webrtc_connect's
// unitree_cloud.py.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

const REGIONS: [(&str, &str); 2] = [("global", "https://global-robot-api.unitree.com/"), ("cn", "https://robot-api.unitree.com/")];
const APP_SIGN_SECRET: &str = "XyvkwK45hp5PHfA8";
// copied from the apk verbatim — the cloud is picky (a wrong AppVersion flips the response from code 100 to 1003)
const BASE_HEADERS: [(&str, &str); 9] = [
    ("DeviceId", "Samsung/Samsung/SM-S931B/s24/14/34"),
    ("DevicePlatform", "Android"),
    ("DeviceModel", "SM-S931B"),
    ("SystemVersion", "34"),
    ("AppVersion", "1.11.4"),
    ("AppLocale", "en_US"),
    ("Channel", "UMENG_CHANNEL"),
    (
        "User-Agent",
        "Mozilla/5.0 (Linux; Android 14; SM-S931B Build/AP3A.240905.015.A2; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/127.0.6533.103 Mobile Safari/537.36",
    ),
    ("Content-Type", "application/x-www-form-urlencoded"),
];

#[derive(Clone, Debug)]
pub struct BoundRobot {
    pub sn: String,
    pub alias: String,
    pub key: String,
}

fn md5_hex(text: &str) -> String {
    format!("{:x}", md5::compute(text))
}

fn timezone() -> String {
    std::process::Command::new("date")
        .arg("+%Z")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "UTC".into())
}

fn form(params: &[(&str, &str)]) -> String {
    params.iter().map(|(key, value)| format!("{}={}", encode(key), encode(value))).collect::<Vec<_>>().join("&")
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

async fn call(
    client: &reqwest::Client,
    base: &str,
    app_name: &str,
    method: &str,
    path: &str,
    params: &[(&str, &str)],
    token: &str,
) -> Result<Value, String> {
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().to_string();
    let nonce: String = (0..32).map(|_| format!("{:x}", rand::random::<u8>() % 16)).collect();
    let body = form(params);
    let mut request =
        if method == "GET" { client.get(format!("{base}{path}?{body}")) } else { client.post(format!("{base}{path}")).body(body) };
    for (key, value) in BASE_HEADERS {
        request = request.header(key, value);
    }
    request = request
        .header("AppTimezone", timezone())
        .header("AppTimestamp", &ts)
        .header("AppNonce", &nonce)
        .header("AppSign", md5_hex(&format!("{APP_SIGN_SECRET}{ts}{nonce}")))
        .header("AppName", app_name)
        .header("Token", token);
    let response = request.send().await.map_err(|e| format!("Unitree cloud {path}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Unitree cloud {path}: HTTP {}", response.status().as_u16()));
    }
    let result: Value = response.json().await.map_err(|e| format!("Unitree cloud {path}: {e}"))?;
    if result["code"] != 100 {
        let message = result["errorMsg"].as_str().map(|m| format!(": {m}")).unwrap_or_default();
        return Err(format!("Unitree cloud {path} failed (code {}{message})", result["code"]));
    }
    Ok(result["data"].clone())
}

/// Signs in and lists every robot bound to the account. One account has a separate binding list per region, and the
/// list depends on the AppName header (the G1 app's includes G1s the Go2 app's doesn't), so every combination is
/// queried and merged by serial. Only when every region rejects the login is that an error.
pub async fn fetch_bound_robots(email: &str, password: &str) -> Result<Vec<BoundRobot>, String> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|e| e.to_string())?;
    let mut by_sn: BTreeMap<String, BoundRobot> = BTreeMap::new();
    let mut last_error = None;
    let hashed = md5_hex(password);
    for (_, base) in REGIONS {
        let token = match call(&client, base, "Go2", "POST", "login/email", &[("email", email), ("password", &hashed)], "").await {
            Ok(login) => login["accessToken"].as_str().unwrap_or("").to_string(),
            Err(err) => {
                last_error = Some(err);
                continue;
            }
        };
        for app_name in ["Go2", "G1"] {
            let devices = call(&client, base, app_name, "GET", "device/bind/list", &[], &token).await?;
            for device in devices.as_array().cloned().unwrap_or_default() {
                let sn = device["sn"].as_str().unwrap_or("").to_string();
                if sn.is_empty() {
                    continue;
                }
                let previous = by_sn.get(&sn).cloned().unwrap_or(BoundRobot { sn: sn.clone(), alias: String::new(), key: String::new() });
                let alias = device["alias"].as_str().filter(|a| !a.is_empty()).map(str::to_string).unwrap_or(previous.alias);
                let key = device["key"]
                    .as_str()
                    .filter(|k| !k.is_empty())
                    .or(device["gcm_key"].as_str().filter(|k| !k.is_empty()))
                    .map(str::to_string)
                    .unwrap_or(previous.key);
                by_sn.insert(sn.clone(), BoundRobot { sn, alias, key });
            }
        }
    }
    match (by_sn.is_empty(), last_error) {
        (true, Some(err)) => Err(err),
        _ => Ok(by_sn.into_values().collect()),
    }
}
