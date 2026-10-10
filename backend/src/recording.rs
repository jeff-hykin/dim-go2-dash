// The Record button: a dimos memory store (.db, the default) or an mcap of the drive session's WebRTC traffic (record.rs
// writes it), in Desktop's recordings folder under `go2/`, named `<local date>_<time>_<dog name>_<machine id>.db`. What
// goes in (the mcap's topics; a .db's streams drop the slash, msg.rs has their dimos types):
//   /color_image /camera_info  the camera (camera.rs)        /lidar /odom /tf /imu /battery /joint_states  (sensors.rs)
//   /joystick  sensor_msgs/Joy: the gamepad's RAW axes and buttons (never velocities; layout in the channel metadata)
//   /cmd_vel   geometry_msgs/Twist: the velocity actually sent to the dog (m/s, rad/s)
//   /robot_action  std_msgs/String: each sport command sent (stand, sit, …) as JSON
// A recording lives as long as the drive session: disconnecting ends it (and the file is finished).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::api::HttpError;
use crate::app::{now_ms, App};
use crate::camera::{CameraTap, MockCamera};
use crate::cdr::Header;
use crate::msg::Msg;
use crate::record::{now_ns, Recorder};
use crate::robot_rtc::DataMessage;

/// the sub-folder of Desktop's recordings folder this app records into (Recordings lists it as `go2/…`)
pub const FOLDER: &str = "go2";

/// The gamepad layout a /joystick message follows (the W3C "standard" mapping, as the Gamepad API reports it: the
/// Steam Deck under Steam, Xbox and PlayStation pads). The same layout Stash's gamepad cockpit records.
pub const JOY_AXES: &str = "0 left stick X (right +), 1 left stick Y (down +), 2 right stick X (right +), 3 right stick Y (down +)";
pub const JOY_BUTTONS: &str = "0 A, 1 B, 2 X, 3 Y, 4 LB, 5 RB, 6 LT, 7 RT, 8 Back/View, 9 Start/Menu, 10 left stick press, \
11 right stick press, 12 d-pad up, 13 d-pad down, 14 d-pad left, 15 d-pad right, 16 Home/Steam";

pub fn channel_metadata(topic: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    match topic {
        "/joystick" => {
            m.insert("description".into(), "raw gamepad input from the Gamepad API: axes -1..1, buttons 0/1 (not velocities)".into());
            m.insert("mapping".into(), "standard (w3.org/TR/gamepad), rounded to 3 decimals, no dead zone".into());
            m.insert("axes".into(), JOY_AXES.into());
            m.insert("buttons".into(), JOY_BUTTONS.into());
        }
        "/cmd_vel" => {
            m.insert("description".into(), "the velocity command sent to the dog (sport Move), m/s and rad/s; zeros on stop".into());
        }
        "/robot_action" => {
            m.insert("description".into(), "each sport command sent to the dog, as JSON {name, sends, sent, dryRun}".into());
        }
        "/lidar" => {
            m.insert("description".into(), "the Go2's local voxel map (rt/utlidar/voxel_map_compressed), decoded to points".into());
        }
        _ => {}
    }
    m.insert("source".into(), "dim-go2-dash (Go2 Ctrl) over the robot's WebRTC session".into());
    m
}

pub struct Active {
    pub recorder: Arc<Recorder>,
    pub file: String,
    pub robot: String,
    pub dry: bool,
    camera: Mutex<Option<CameraTap>>,
    last_battery_ns: AtomicU64,
    camera_drops: AtomicU64,
    ended: AtomicBool,
}

impl Active {
    pub fn on_data(&self, message: &DataMessage) {
        let now = now_ns();
        let due = now.saturating_sub(self.last_battery_ns.load(Ordering::Relaxed)) >= crate::sensors::BATTERY_EVERY_NS;
        let out = crate::sensors::convert(&message.topic, &message.json, message.binary.as_deref(), now, due);
        for item in out {
            if item.topic == "/battery" {
                self.last_battery_ns.store(now, Ordering::Relaxed);
            }
            self.recorder.write(item.topic, item.msg);
        }
    }

    pub fn on_rtp(&self, packet: webrtc::rtp::packet::Packet) {
        if let Some(tap) = self.camera.lock().unwrap().as_ref() {
            if !tap.push(packet) {
                self.camera_drops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn cmd_vel(&self, x: f64, y: f64, yaw: f64) {
        self.recorder.write("/cmd_vel", Msg::Twist([x, y, 0.0], [0.0, 0.0, yaw]));
    }

    pub fn command(&self, record: &Value) {
        self.recorder.write("/robot_action", Msg::Text(record.to_string()));
    }

    pub fn joy(&self, axes: &[f32], buttons: &[i32]) {
        self.recorder.write("/joystick", Msg::Joy(Header { stamp_ns: now_ns(), frame_id: "" }, axes.to_vec(), buttons.to_vec()));
    }

    pub fn status(&self) -> Value {
        let mut status = self.recorder.status();
        status["robot"] = json!(self.robot);
        status["dryRun"] = json!(self.dry);
        status["cameraDrops"] = json!(self.camera_drops.load(Ordering::Relaxed));
        status
    }
}

/// Where recordings go: DIMOS_APP's recordingsDir/go2 (GO2_DASH_RECORDINGS_DIR overrides), else <data dir>/recordings.
pub fn recordings_dir(app: &App) -> PathBuf {
    let options = app.record_options();
    if !options.directory.is_empty() {
        return PathBuf::from(options.directory);
    }
    if let Some(dir) = std::env::var_os("GO2_DASH_RECORDINGS_DIR") {
        return PathBuf::from(dir);
    }
    match &app.recordings_root {
        Some(root) => root.join(FOLDER),
        None => app.data_dir().join("recordings"),
    }
}

impl App {
    pub fn recording(&self) -> Option<Arc<Active>> {
        self.recording.lock().unwrap().clone()
    }

    pub fn record_state(&self) -> Value {
        match self.recording() {
            Some(active) => active.status(),
            None => json!({ "active": false }),
        }
    }

    fn publish_record(&self) {
        self.publish(json!({ "type": "record", "record": self.record_state() }));
    }

    /// Starts recording the open drive session.
    pub async fn record_start(self: &Arc<Self>) -> Result<Value, HttpError> {
        let drive = self.drive.lock().await.clone().ok_or_else(|| HttpError::conflict("connect to a dog first (Drive), then Record"))?;
        if let Some(active) = self.recording() {
            return Ok(active.status());
        }
        let dir = recordings_dir(self);
        let started = now_ms();
        let options = self.record_options();
        let stamp = crate::record::local_stamp(started);
        let name = crate::record::file_name(&stamp, &drive.name, &crate::record::machine_id(), &options.format);
        let mut path = dir.join(&name);
        let mut n = 2;
        while path.exists() {
            path = dir.join(name.replace(&format!(".{}", options.format), &format!("-{n}.{}", options.format)));
            n += 1;
        }
        let metadata = BTreeMap::from([
            ("app".to_string(), "dim-go2-dash".to_string()),
            ("app_version".to_string(), env!("CARGO_PKG_VERSION").to_string()),
            ("robot_type".to_string(), "unitree_go2".to_string()),
            ("robot_name".to_string(), drive.name.clone()),
            ("robot_serial".to_string(), drive.robot.clone().unwrap_or_default()),
            ("robot_ip".to_string(), drive.ip.clone()),
            ("dry_run".to_string(), drive.dry.to_string()),
            ("mock".to_string(), self.mock.to_string()),
            ("started_ms".to_string(), started.to_string()),
        ]);
        let recorder =
            Arc::new(Recorder::start_with_options(&path, metadata, channel_metadata, options).map_err(HttpError::bad)?);
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        let weak_drive = Arc::downgrade(&drive);
        let want_keyframe: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(drive) = weak_drive.upgrade() {
                tokio::runtime::Handle::try_current().ok().map(|rt| rt.spawn(async move { drive.keyframe().await }));
            }
        });
        let active = Arc::new(Active {
            recorder: recorder.clone(),
            file: file.clone(),
            robot: drive.name.clone(),
            dry: drive.dry,
            camera: Mutex::new(Some(crate::camera::start(recorder, want_keyframe))),
            last_battery_ns: AtomicU64::new(0),
            camera_drops: AtomicU64::new(0),
            ended: AtomicBool::new(false),
        });
        self.index_update(&file, |entry| {
            entry["robot"] = json!(drive.name);
            entry["serial"] = json!(drive.robot);
            entry["startedAt"] = json!(started);
            entry["dryRun"] = json!(drive.dry);
            entry["mock"] = json!(self.mock);
        });
        *self.recording.lock().unwrap() = Some(active.clone());
        drive.start_streams().await;
        if self.mock {
            spawn_mock_robot(self.clone(), active.clone());
        }
        self.publish_record();
        self.publish_recordings();
        Ok(active.status())
    }

    /// Stops recording: finishes the file (summary + footer), then auto-uploads it when that's on.
    pub async fn record_stop(self: &Arc<Self>) -> Result<Value, HttpError> {
        let Some(active) = self.recording.lock().unwrap().take() else {
            return Err(HttpError::conflict("not recording"));
        };
        active.ended.store(true, Ordering::Relaxed);
        if let Some(drive) = self.drive.lock().await.clone() {
            drive.stop_streams().await;
        }
        // the camera thread holds the recorder: end it first so its last frames are in
        drop(active.camera.lock().unwrap().take());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let recorder = active.recorder.clone();
        let status = tokio::task::spawn_blocking(move || recorder.finish()).await.map_err(|e| HttpError::new(500, e.to_string()))?;
        let status = status.map_err(|e| HttpError::new(500, e))?;
        self.index_update(&active.file, |entry| {
            entry["endedAt"] = json!(now_ms());
            entry["messages"] = status["messages"].clone();
            entry["topics"] = status["topics"].clone();
            entry["dropped"] = status["dropped"].clone();
        });
        self.publish_record();
        self.publish_recordings();
        self.clone().auto_upload(active.file.clone());
        Ok(json!({ "file": active.file, "path": active.recorder.path.display().to_string(), "status": status }))
    }

    /// A gamepad sample from the page (raw axes / buttons), recorded on /joystick while recording.
    pub fn joy(&self, axes: Vec<f32>, buttons: Vec<i32>) -> Value {
        match self.recording() {
            Some(active) => {
                active.joy(&axes, &buttons);
                json!({ "recorded": true })
            }
            None => json!({ "recorded": false }),
        }
    }

    /// Finishes recordings a killed run left without a summary (so every tool can read them), once at start.
    pub fn recover_recordings(&self) {
        let dir = recordings_dir(self);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if crate::record::is_recording_file(&path) && !crate::record::is_finished(&path) {
                match crate::record::recover(&path) {
                    Ok(count) => {
                        eprintln!("recovered {} ({count} messages): it was left unfinished", path.display());
                        let file = entry.file_name().to_string_lossy().into_owned();
                        self.index_update(&file, |e| e["recovered"] = json!(true));
                    }
                    Err(err) => eprintln!("couldn't recover {}: {err}", path.display()),
                }
            }
        }
    }
}

pub fn is_recording(app: &App, path: &Path) -> bool {
    app.recording().is_some_and(|active| active.recorder.path == path)
}

/// GO2_DASH_MOCK's robot while recording: the topics a Go2 sends (as JSON / binary frames, through the same converters),
/// a camera (H.264 RTP through the same decoder), and a pose that follows the drive commands.
fn spawn_mock_robot(app: Arc<App>, active: Arc<Active>) {
    let app = Arc::downgrade(&app);
    tokio::spawn(async move {
        let mut camera = tokio::task::spawn_blocking(MockCamera::new).await.ok().flatten();
        let (mut x, mut y, mut yaw) = (0.0f64, 0.0f64, 0.0f64);
        let mut tick = 0u64;
        let period = Duration::from_millis(1000 / 15);
        while !active.ended.load(Ordering::Relaxed) {
            tokio::time::sleep(period).await;
            let Some(app) = app.upgrade() else { break };
            tick += 1;
            let dt = period.as_secs_f64();
            if let Some((vx, vy, wz)) = app.drive.lock().await.as_ref().map(|d| d.commanded_velocity()) {
                x += (vx * yaw.cos() - vy * yaw.sin()) * dt;
                y += (vx * yaw.sin() + vy * yaw.cos()) * dt;
                yaw += wz * dt;
            }
            if let Some(cam) = camera.as_mut() {
                // encoding a frame takes a few ms: off the async workers' hot path
                let packets = cam.next_packets("mock");
                for packet in packets {
                    active.on_rtp(packet);
                }
            }
            let (qz, qw) = ((yaw / 2.0).sin(), (yaw / 2.0).cos());
            let pose = json!({ "type": "msg", "topic": crate::sensors::POSE, "data": {
                "header": { "stamp": { "sec": 0, "nanosec": 0 }, "frame_id": "odom" },
                "pose": { "position": { "x": x, "y": y, "z": 0.3 }, "orientation": { "x": 0.0, "y": 0.0, "z": qz, "w": qw } } } });
            active.on_data(&DataMessage { topic: crate::sensors::POSE.into(), json: pose, binary: None });
            let sport = json!({ "type": "msg", "topic": crate::sensors::SPORT_STATE, "data": { "imu_state": {
                "quaternion": [qw, 0.0, 0.0, qz], "gyroscope": [0.0, 0.0, 0.0], "accelerometer": [0.0, 0.0, 9.81], "rpy": [0.0, 0.0, yaw] } } });
            active.on_data(&DataMessage { topic: crate::sensors::SPORT_STATE.into(), json: sport, binary: None });
            let low = json!({ "type": "msg", "topic": crate::sensors::LOWSTATE, "data": {
                "motor_state": (0..20).map(|i| json!({ "q": if i < 12 { [0.0, 0.8, -1.5][i % 3] } else { 0.0 }, "temperature": 35, "lost": 0 })).collect::<Vec<_>>(),
                "bms_state": { "soc": 80, "current": -2000, "cycle": 10, "bq_ntc": [30, 29], "mcu_ntc": [33, 32] },
                "power_v": 28.3, "foot_force": [90, 90, 90, 90] } });
            active.on_data(&DataMessage { topic: crate::sensors::LOWSTATE.into(), json: low, binary: None });
            if tick % 2 == 0 {
                // a ring of "walls" around the start point, in the robot's 128×128 window centred on it
                let mut voxels = vec![0u8; 0x800 * 20];
                for z in 0..20usize {
                    for i in 0..128usize {
                        for (vx, vy) in [(i, 10usize), (i, 117), (10, i), (117, i)] {
                            let index = z * 0x800 + vy * 0x10 + vx / 8;
                            voxels[index] |= 0x80 >> (vx % 8);
                        }
                    }
                }
                let origin = [x - 3.2, y - 3.2, 0.0];
                if let Some(message) = crate::robot_rtc::parse_binary(&crate::sensors::lidar_message(&voxels, origin)) {
                    active.on_data(&message);
                }
            }
        }
    });
}
