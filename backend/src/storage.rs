// The recordings' disk never fills by surprise: below FREE_FLOOR free, with auto-upload on, the oldest recording that's
// uploaded is deleted (never one that isn't); with none left to delete, the page shows a warning with the recording
// time left. Every deletion goes in the log and /logs.

use crate::app::App;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// free space below this share of the disk starts the cleanup (Jeff 2026-10-27: "within 20% full")
const FREE_FLOOR: f64 = 0.20;
const CHECK_EVERY: Duration = Duration::from_secs(30);

/// (free, total) bytes of the filesystem holding `path`
fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    Some((stat.f_bavail as u64 * block, stat.f_blocks as u64 * block))
}

/// The recordings that may be deleted, oldest first: uploaded and not recording (`list` is recordings_list's, newest first).
fn deletable_oldest_first(list: &[Value]) -> Vec<String> {
    list.iter()
        .rev()
        .filter(|r| r["upload"]["state"] == "done" && r["recording"] != true)
        .filter_map(|r| r["file"].as_str().map(String::from))
        .collect()
}

/// Bytes a second recording writes: the one running now (once it's run 30 s), else the recent finished ones together.
fn recording_rate(list: &[Value], now_ms: u64) -> Option<f64> {
    if let Some(active) = list.iter().find(|r| r["recording"] == true) {
        let seconds = now_ms.saturating_sub(active["startedAt"].as_u64()?) as f64 / 1000.0;
        if seconds >= 30.0 {
            return Some(active["bytes"].as_f64()? / seconds);
        }
    }
    let recent: Vec<(f64, f64)> = list
        .iter()
        .filter_map(|r| Some((r["bytes"].as_f64()?, r["seconds"].as_f64()?)))
        .filter(|&(bytes, seconds)| seconds >= 30.0 && bytes > 0.0)
        .take(10)
        .collect();
    let (bytes, seconds) = recent.iter().fold((0.0, 0.0), |(b, s), &(rb, rs)| (b + rb, s + rs));
    (seconds > 0.0).then(|| bytes / seconds)
}

impl App {
    /// {freeBytes, totalBytes, low, secondsLeft (of recording, at the recent rate; null before any), notUploaded, autoUpload}
    pub fn storage_state(&self) -> Value {
        self.storage.lock().unwrap().clone()
    }

    /// Checks the recordings' disk now and every CHECK_EVERY.
    pub fn watch_storage(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.check_storage().await;
                tokio::time::sleep(CHECK_EVERY).await;
            }
        });
    }

    async fn check_storage(self: &Arc<Self>) {
        let dir = crate::recording::recordings_dir(self);
        let Some((mut free, total)) = disk_space(&dir) else { return };
        let floor = (FREE_FLOOR * total as f64) as u64;
        let auto_upload = self.auto_upload_on();
        if free < floor && auto_upload {
            for file in deletable_oldest_first(&self.recordings_list()) {
                if free >= floor {
                    break;
                }
                match self.delete_recording(&file).await {
                    Ok(_) => {
                        let before = free;
                        free = disk_space(&dir).map_or(free, |(f, _)| f);
                        crate::log::line(&format!("storage: deleted uploaded {file} ({} MB freed, {} GB free)", (free.saturating_sub(before)) >> 20, free >> 30));
                        self.publish(json!({ "type": "storage_deleted", "file": file, "freeBytes": free, "totalBytes": total }));
                    }
                    Err(err) => crate::log::line(&format!("storage: couldn't delete {file}: {}", err.message)),
                }
            }
        }
        let list = self.recordings_list();
        let low = free < floor;
        let state = json!({
            "freeBytes": free,
            "totalBytes": total,
            "low": low,
            "secondsLeft": recording_rate(&list, crate::app::now_ms()).map(|rate| (free as f64 / rate) as u64),
            "notUploaded": list.iter().filter(|r| r["upload"]["state"] != "done").count(),
            "autoUpload": auto_upload,
        });
        let was_low = std::mem::replace(&mut *self.storage.lock().unwrap(), state.clone())["low"] == true;
        if low && !was_low {
            crate::log::line(&format!("storage: LOW — {} GB free of {} and nothing uploaded left to delete: {state}", free >> 30, total >> 30));
        }
        if low || was_low {
            self.publish(json!({ "type": "storage", "storage": state }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_only_uploaded_oldest_first() {
        // newest first, as recordings_list gives it
        let list = vec![
            json!({ "file": "d", "upload": { "state": "done" }, "recording": true }),
            json!({ "file": "c", "upload": { "state": "done" }, "recording": false }),
            json!({ "file": "b", "upload": { "state": "queued" }, "recording": false }),
            json!({ "file": "a", "upload": null, "recording": false }),
            json!({ "file": "z", "upload": { "state": "done" }, "recording": false }),
        ];
        assert_eq!(deletable_oldest_first(&list), vec!["z", "c"]);
    }

    #[test]
    fn rate_from_the_running_recording_else_the_recent_ones() {
        let running = vec![json!({ "recording": true, "startedAt": 0, "bytes": 6_000_000 }), json!({ "bytes": 1.0, "seconds": 60.0 })];
        assert_eq!(recording_rate(&running, 60_000), Some(100_000.0));
        let finished = vec![json!({ "bytes": 3_000_000, "seconds": 60.0 }), json!({ "bytes": 1_000_000, "seconds": 40.0 })];
        assert_eq!(recording_rate(&finished, 0), Some(40_000.0));
        assert_eq!(recording_rate(&[], 0), None);
    }
}
