// This computer's battery — the Steam Deck or laptop driving the dog — beside the dog's own (which comes with its
// lowstate, drive.rs). Linux reads sysfs (`/sys/class/power_supply`), macOS asks `pmset -g batt`; a desktop has none.
use std::path::Path;

use serde_json::{json, Value};

/// `{percent, status, plugged, watts}`: status charging | discharging | full | not-charging | unknown; watts the draw
/// (or charge) when the driver says; None without a battery.
pub fn host_battery() -> Option<Value> {
    #[cfg(target_os = "linux")]
    {
        from_sysfs(Path::new("/sys/class/power_supply"))
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("pmset").args(["-g", "batt"]).output().ok()?;
        parse_pmset(&String::from_utf8_lossy(&out.stdout))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// `api/state.host`: this computer's name and battery. Mock mode: a deck at 64 % on battery, so the pane has two rows.
pub fn host_state(mock: bool) -> Value {
    let name = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty());
    let battery = if mock {
        Some(json!({ "percent": 64, "status": "discharging", "plugged": false, "watts": 11.4 }))
    } else {
        host_battery()
    };
    json!({ "name": name, "battery": battery })
}

/// The first `type` = Battery under `root` (one per supply, as the kernel lays them out); any other supply that is
/// `online` means plugged in. Power in µW (`power_now`), else µA × µV.
pub fn from_sysfs(root: &Path) -> Option<Value> {
    let read = |dir: &Path, file: &str| std::fs::read_to_string(dir.join(file)).ok().map(|s| s.trim().to_string());
    let number = |dir: &Path, file: &str| read(dir, file).and_then(|s| s.parse::<f64>().ok());
    let mut plugged = false;
    let mut battery: Option<Value> = None;
    let mut dirs: Vec<_> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        if read(&dir, "type").as_deref() != Some("Battery") {
            plugged |= read(&dir, "online").as_deref() == Some("1");
            continue;
        }
        if battery.is_some() {
            continue;
        }
        let Some(percent) = number(&dir, "capacity") else { continue };
        let status = match read(&dir, "status").as_deref() {
            Some("Charging") => "charging",
            Some("Discharging") => "discharging",
            Some("Full") => "full",
            Some("Not charging") => "not-charging",
            _ => "unknown",
        };
        let watts = number(&dir, "power_now")
            .map(|uw| uw / 1e6)
            .or_else(|| Some(number(&dir, "current_now")? * number(&dir, "voltage_now")? / 1e12))
            .map(|w| (w * 10.0).round() / 10.0);
        battery = Some(json!({ "percent": percent.clamp(0.0, 100.0) as u8, "status": status, "watts": watts }));
    }
    let mut battery = battery?;
    battery["plugged"] = json!(plugged || matches!(battery["status"].as_str(), Some("charging" | "full")));
    Some(battery)
}

/// `pmset -g batt`: "Now drawing from 'Battery Power'" then " -InternalBattery-0 (id=123)\t85%; discharging; 3:12 remaining".
pub fn parse_pmset(text: &str) -> Option<Value> {
    let plugged = text.contains("'AC Power'");
    let line = text.lines().find(|l| l.contains("InternalBattery"))?;
    let after_tab = line.split('\t').nth(1).unwrap_or(line);
    let mut parts = after_tab.split(';').map(str::trim);
    let percent: u8 = parts.next()?.trim_end_matches('%').parse().ok()?;
    let status = match parts.next().unwrap_or("") {
        "charging" => "charging",
        "discharging" => "discharging",
        "charged" | "finishing charge" => "full",
        "AC attached" => "not-charging",
        _ => "unknown",
    };
    Some(json!({ "percent": percent, "status": status, "plugged": plugged || status == "charging" || status == "full", "watts": Value::Null }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, value) in files {
            std::fs::write(dir.join(file), format!("{value}\n")).unwrap();
        }
    }

    #[test]
    fn a_deck_on_battery_then_plugged_in() {
        let root = std::env::temp_dir().join(format!("go2-batt-{}", crate::app::now_ms()));
        supply(&root, "ACAD", &[("type", "Mains"), ("online", "0")]);
        supply(&root, "BAT1", &[("type", "Battery"), ("capacity", "42"), ("status", "Discharging"), ("power_now", "12345000")]);
        assert_eq!(from_sysfs(&root).unwrap(), json!({ "percent": 42, "status": "discharging", "plugged": false, "watts": 12.3 }));
        // the charger goes in: µA × µV when the driver has no power_now
        supply(&root, "ACAD", &[("online", "1")]);
        supply(&root, "BAT1", &[("status", "Charging"), ("capacity", "43")]);
        std::fs::remove_file(root.join("BAT1/power_now")).unwrap();
        supply(&root, "BAT1", &[("current_now", "1500000"), ("voltage_now", "7800000")]);
        assert_eq!(from_sysfs(&root).unwrap(), json!({ "percent": 43, "status": "charging", "plugged": true, "watts": 11.7 }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_desktop_has_no_battery() {
        let root = std::env::temp_dir().join(format!("go2-nobatt-{}", crate::app::now_ms()));
        supply(&root, "ADP1", &[("type", "Mains"), ("online", "1")]);
        assert!(from_sysfs(&root).is_none());
        assert!(from_sysfs(Path::new("/nonexistent/power_supply")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pmset_lines() {
        let text = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4456547)\t85%; discharging; 3:12 remaining present: true\n";
        assert_eq!(parse_pmset(text).unwrap(), json!({ "percent": 85, "status": "discharging", "plugged": false, "watts": null }));
        let text = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4456547)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(parse_pmset(text).unwrap()["plugged"], true);
        assert!(parse_pmset("Now drawing from 'AC Power'\n").is_none());
    }
}
