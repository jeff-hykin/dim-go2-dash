// This app's recordings list (the files in recordingsDir/go2, newest first) and their uploads. Uploads go through
// Desktop's upload queue (the dimos gateway: POST /dimos/uploads, DELETE /dimos/uploads/{id}, …), which sends them to
// the Dimensional cloud; this keeps, per recording, the upload id and what Desktop says about it (polled while one
// runs). Auto-upload (Settings): each finished recording is queued; a failure is retried (with a growing wait), and
// while this computer is offline it waits instead of failing.
//
// The index (go2_dash_recordings.json in the data dir) holds what a file can't: robot, start/end, upload state.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::api::HttpError;
use crate::app::{now_ms, App};
use crate::recording::recordings_dir;

pub const INDEX_FILE: &str = "go2_dash_recordings.json";
pub const SETTINGS_FILE: &str = "go2_dash_settings.json";
const POLL: Duration = Duration::from_millis(1000);
/// retry waits for a failed auto-upload: 10 s, 30 s, 1 min, 2 min, then every 5 min
const RETRY_WAITS: [u64; 5] = [10, 30, 60, 120, 300];

impl App {
    pub(crate) fn index(&self) -> Map<String, Value> {
        self.recordings_index.lock().unwrap().clone()
    }

    pub(crate) fn index_update(&self, file: &str, update: impl FnOnce(&mut Value)) {
        let snapshot = {
            let mut index = self.recordings_index.lock().unwrap();
            let entry = index.entry(file.to_string()).or_insert_with(|| json!({}));
            update(entry);
            Value::Object(index.clone())
        };
        self.save_file(INDEX_FILE, snapshot);
    }

    pub(crate) fn publish_recordings(&self) {
        self.publish(json!({ "type": "recordings", "recordings": self.recordings_list() }));
    }

    pub fn settings(&self) -> Value {
        self.settings.lock().unwrap().clone()
    }

    pub fn update_settings(self: &Arc<Self>, auto_upload: Option<bool>) -> Value {
        let settings = {
            let mut settings = self.settings.lock().unwrap();
            if let Some(on) = auto_upload {
                settings["autoUpload"] = json!(on);
            }
            settings.clone()
        };
        self.save_file(SETTINGS_FILE, settings.clone());
        self.publish(json!({ "type": "settings", "settings": settings }));
        settings
    }

    fn auto_upload_on(&self) -> bool {
        self.settings.lock().unwrap()["autoUpload"].as_bool().unwrap_or(false)
    }

    /// Newest first: name, file, size, when, robot, duration, whether it's recording now, and its upload.
    pub fn recordings_list(&self) -> Vec<Value> {
        let dir = recordings_dir(self);
        let index = self.index();
        let root = self.recordings_root.clone();
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut list: Vec<Value> = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "mcap"))
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                let file = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                let info = index.get(&file).cloned().unwrap_or(json!({}));
                let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
                let started = info["startedAt"].as_u64().unwrap_or(modified);
                let recording = crate::recording::is_recording(self, &path);
                let ended = info["endedAt"].as_u64();
                // Desktop's id for it: its path under the recordings folder (what Recordings' #/replay/<id> takes)
                let id = root.as_ref().and_then(|root| path.strip_prefix(root).ok()).map(|p| p.to_string_lossy().into_owned());
                Some(json!({
                    "file": file,
                    "name": file.trim_end_matches(".mcap"),
                    "path": path.display().to_string(),
                    "id": id,
                    "bytes": meta.len(),
                    "startedAt": started,
                    "seconds": ended.map(|end| (end.saturating_sub(started)) as f64 / 1000.0),
                    "robot": info["robot"],
                    "recording": recording,
                    "recovered": info["recovered"].as_bool().unwrap_or(false),
                    "mock": info["mock"].as_bool().unwrap_or(false),
                    "messages": info["messages"],
                    "upload": info.get("upload").cloned().unwrap_or(Value::Null),
                }))
            })
            .collect();
        list.sort_by(|a, b| {
            b["startedAt"].as_u64().cmp(&a["startedAt"].as_u64()).then_with(|| b["file"].as_str().cmp(&a["file"].as_str()))
        });
        list
    }

    fn recording_path(&self, file: &str) -> Result<PathBuf, HttpError> {
        if file.is_empty() || file.contains('/') || file.contains('\\') || file.starts_with('.') || !file.ends_with(".mcap") {
            return Err(HttpError::bad("not a recording of this app"));
        }
        let path = recordings_dir(self).join(file);
        if !path.is_file() {
            return Err(HttpError::not_found(format!("no recording {file}")));
        }
        Ok(path)
    }

    pub fn rename_recording(&self, file: &str, name: &str) -> Result<Value, HttpError> {
        let path = self.recording_path(file)?;
        if crate::recording::is_recording(self, &path) {
            return Err(HttpError::conflict("it's still recording: stop it first"));
        }
        if self.upload_running(file) {
            return Err(HttpError::conflict("it's uploading: cancel the upload or wait for it to finish"));
        }
        let safe = crate::record::safe_name(name.trim().trim_end_matches(".mcap"));
        if safe.is_empty() {
            return Err(HttpError::bad("the name needs letters or digits"));
        }
        let new_file = format!("{safe}.mcap");
        if new_file == file {
            return Ok(json!({ "file": file }));
        }
        let new_path = path.with_file_name(&new_file);
        if new_path.exists() {
            return Err(HttpError::conflict(format!("a recording named {safe} already exists")));
        }
        std::fs::rename(&path, &new_path).map_err(|e| HttpError::new(500, e.to_string()))?;
        let snapshot = {
            let mut index = self.recordings_index.lock().unwrap();
            if let Some(entry) = index.remove(file) {
                index.insert(new_file.clone(), entry);
            }
            Value::Object(index.clone())
        };
        self.save_file(INDEX_FILE, snapshot);
        self.publish_recordings();
        Ok(json!({ "file": new_file }))
    }

    pub async fn delete_recording(self: &Arc<Self>, file: &str) -> Result<Value, HttpError> {
        let path = self.recording_path(file)?;
        if crate::recording::is_recording(self, &path) {
            return Err(HttpError::conflict("it's still recording: stop it first"));
        }
        if self.upload_running(file) {
            let _ = self.cancel_upload(file).await;
        }
        std::fs::remove_file(&path).map_err(|e| HttpError::new(500, e.to_string()))?;
        let snapshot = {
            let mut index = self.recordings_index.lock().unwrap();
            index.remove(file);
            Value::Object(index.clone())
        };
        self.save_file(INDEX_FILE, snapshot);
        self.publish_recordings();
        Ok(json!({ "deleted": file }))
    }

    fn upload_running(&self, file: &str) -> bool {
        let index = self.recordings_index.lock().unwrap();
        index.get(file).is_some_and(|e| matches!(e["upload"]["state"].as_str(), Some("queued" | "uploading" | "waiting")))
    }

    // ── uploads, through Desktop ──

    /// Queues a recording on Desktop's upload queue. `auto`: queued by auto-upload (it retries by itself).
    pub async fn upload_recording(self: &Arc<Self>, file: &str, auto: bool) -> Result<Value, HttpError> {
        let path = self.recording_path(file)?;
        if crate::recording::is_recording(self, &path) {
            return Err(HttpError::conflict("it's still recording: stop it first, then upload"));
        }
        if self.mock && self.desktop_url.get().is_none() {
            // mock without Desktop: pretend, so the UI can be tried
            self.set_upload(file, json!({ "state": "done", "id": "mock", "bytesDone": 1, "bytesTotal": 1, "auto": auto, "at": now_ms() }));
            return Ok(json!({ "upload": self.index()[file]["upload"] }));
        }
        if auto && !online().await {
            self.set_upload(
                file,
                json!({ "state": "offline", "error": "offline: it uploads when the network is back", "auto": true, "at": now_ms() }),
            );
            self.clone().watch_uploads();
            return Ok(json!({ "upload": self.index()[file]["upload"] }));
        }
        let upload = self
            .desktop_call(reqwest::Method::POST, "/dimos/uploads", Some(json!({ "path": path.display().to_string(), "kind": "recording" })))
            .await;
        match upload {
            Ok(upload) => {
                self.set_upload(file, from_desktop(&upload, auto));
                self.clone().watch_uploads();
                Ok(json!({ "upload": self.index()[file]["upload"] }))
            }
            Err(err) => {
                self.set_upload(file, json!({ "state": "failed", "error": err.message, "auto": auto, "at": now_ms() }));
                if auto {
                    self.clone().watch_uploads();
                }
                Err(err)
            }
        }
    }

    pub async fn cancel_upload(self: &Arc<Self>, file: &str) -> Result<Value, HttpError> {
        let upload = self.index().get(file).map(|e| e["upload"].clone()).unwrap_or(Value::Null);
        if let Some(id) = upload["id"].as_str().filter(|id| *id != "mock") {
            self.desktop_call(reqwest::Method::DELETE, &format!("/dimos/uploads/{id}"), None).await?;
        }
        self.set_upload(file, json!({ "state": "cancelled", "at": now_ms() }));
        Ok(json!({ "cancelled": file }))
    }

    fn set_upload(&self, file: &str, upload: Value) {
        self.index_update(file, |entry| entry["upload"] = upload);
        self.publish_recordings();
    }

    /// After a recording ends: queue it when auto-upload is on.
    pub fn auto_upload(self: Arc<Self>, file: String) {
        if !self.auto_upload_on() {
            return;
        }
        tokio::spawn(async move {
            let _ = self.upload_recording(&file, true).await;
        });
    }

    /// Follows Desktop's queue while any upload of ours is open (one watcher at a time): progress, done, failed; retries
    /// auto-uploads; waits while offline or signed out.
    pub fn watch_uploads(self: Arc<Self>) {
        if self.upload_watcher.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(POLL).await;
                let open: Vec<(String, Value)> = self
                    .index()
                    .into_iter()
                    .filter(|(_, e)| {
                        let state = e["upload"]["state"].as_str().unwrap_or("");
                        matches!(state, "queued" | "uploading" | "waiting" | "offline" | "signin")
                            || (state == "failed" && e["upload"]["auto"].as_bool() == Some(true) && self.auto_upload_on())
                    })
                    .collect();
                if open.is_empty() {
                    break;
                }
                let queue = match self.desktop_call(reqwest::Method::GET, "/dimos/uploads", None).await {
                    Ok(queue) => queue,
                    Err(_) => continue, // Desktop restarting: ask again
                };
                let by_id: std::collections::HashMap<String, Value> = queue["uploads"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|u| Some((u["id"].as_str()?.to_string(), u.clone()))).collect())
                    .unwrap_or_default();
                let signed_out = queue["waitingForLogin"].as_bool().unwrap_or(false);
                for (file, entry) in open {
                    let ours = &entry["upload"];
                    let auto = ours["auto"].as_bool().unwrap_or(false);
                    let id = ours["id"].as_str().unwrap_or("");
                    let mut next = match by_id.get(id) {
                        Some(theirs) => from_desktop(theirs, auto),
                        None if !id.is_empty() && ours["state"] != "failed" => {
                            json!({ "state": "failed", "error": "Desktop's upload queue no longer has it", "auto": auto })
                        }
                        None => ours.clone(),
                    };
                    if next["state"] == "queued" && signed_out {
                        next["state"] = json!("signin");
                    }
                    if (next["state"] == "failed" || next["state"] == "offline") && auto && self.auto_upload_on() {
                        next = self.retry_plan(&file, ours, next, id).await;
                    }
                    if next != *ours {
                        self.set_upload(&file, next);
                    }
                }
            }
            self.upload_watcher.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    /// A failed auto-upload: offline → wait for the network ("offline"), else retry after a growing wait.
    async fn retry_plan(self: &Arc<Self>, file: &str, ours: &Value, mut next: Value, id: &str) -> Value {
        let attempts = ours["attempts"].as_u64().unwrap_or(0);
        next["attempts"] = json!(attempts);
        next["retryAt"] = ours["retryAt"].clone();
        if !online().await {
            next["state"] = json!("offline");
            next["error"] = json!("offline: it uploads when the network is back");
            return next;
        }
        let wait = RETRY_WAITS[(attempts as usize).min(RETRY_WAITS.len() - 1)] * 1000;
        let retry_at = ours["retryAt"].as_u64().unwrap_or(now_ms() + wait);
        if ours["state"] == "offline" || now_ms() >= retry_at {
            let retried = if id.is_empty() || id == "mock" {
                self.desktop_call(
                    reqwest::Method::POST,
                    "/dimos/uploads",
                    Some(json!({ "path": recordings_dir(self).join(file).display().to_string(), "kind": "recording" })),
                )
                .await
            } else {
                self.desktop_call(reqwest::Method::POST, &format!("/dimos/uploads/{id}/retry"), None).await
            };
            return match retried {
                Ok(upload) => {
                    let mut fresh = from_desktop(&upload, true);
                    fresh["attempts"] = json!(attempts + 1);
                    fresh
                }
                Err(err) => {
                    next["state"] = json!("failed");
                    next["error"] = json!(err.message);
                    next["attempts"] = json!(attempts + 1);
                    next["retryAt"] = json!(now_ms() + RETRY_WAITS[((attempts + 1) as usize).min(RETRY_WAITS.len() - 1)] * 1000);
                    next
                }
            };
        }
        next["retryAt"] = json!(retry_at);
        next
    }
}

/// Desktop's Upload → what the list shows: state (queued, uploading, done, failed, cancelled), progress, error, link.
fn from_desktop(upload: &Value, auto: bool) -> Value {
    json!({
        "id": upload["id"],
        "state": upload["state"],
        "phase": upload["phase"],
        "bytesDone": upload["bytesDone"],
        "bytesTotal": upload["bytesTotal"],
        "rateBps": upload["rateBps"],
        "etaSeconds": upload["etaSeconds"],
        "error": upload["error"].as_str().or(upload["errorCode"].as_str()),
        "errorCode": upload["errorCode"],
        "link": upload["link"],
        "auto": auto,
        "at": now_ms(),
    })
}

/// Whether this computer reaches the internet (a TCP connect to well-known anycast DNS servers, 3 s).
async fn online() -> bool {
    for host in ["1.1.1.1:443", "8.8.8.8:443"] {
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(host)).await {
            return true;
        }
    }
    false
}
