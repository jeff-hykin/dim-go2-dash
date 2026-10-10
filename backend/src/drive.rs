// The live drive session: one WebRTC connection to one Go2 (robot_rtc.rs), its sport commands (stand, sit, jump, …),
// velocity driving, and the camera for the pages (video.rs). One session at a time, like the panel had.
//
// HARDWARE SAFETY: a session opened with `dryRun: true` (or in mock mode) never connects to or sends anything to a
// robot; every command, move and stop also takes `dryRun: true` to say what it would send without sending it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use crate::api::HttpError;
use crate::app::{now_ms, App};
use crate::robot_rtc::{ConnEvent, RobotConn};

const SPORT_DAMP: u32 = 1001;
const SPORT_BALANCE_STAND: u32 = 1002;
const SPORT_STOP_MOVE: u32 = 1003;
const SPORT_STAND_UP: u32 = 1004;
const SPORT_STAND_DOWN: u32 = 1005;
const SPORT_MOVE: u32 = 1008;
const SPORT_POSE: u32 = 1028;
/// after StandUp, wait before BalanceStand so joystick control latches (issued mid-rise it doesn't stick)
const STAND_SETTLE: Duration = Duration::from_millis(3000);
/// velocity envelope: a normalized -1..1 axis maps to these maxima; `run` scales them
const MAX_FORWARD: f64 = 0.6;
const MAX_LATERAL: f64 = 0.4;
const MAX_YAW: f64 = 1.1;
const RUN_MULT: f64 = 2.2;
/// sport Move decays on the robot, so a held velocity is re-sent at this rate
const DRIVE_TICK: Duration = Duration::from_millis(120);
/// the first connect is flaky while the robot frees its stale peer slot: keep retrying this long
const CONNECT_WINDOW: Duration = Duration::from_secs(5);

pub enum Action {
    Stand,
    Pose,
    Sport { api_id: u32, sport_name: &'static str, rests: bool },
}

pub struct Command {
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub action: Action,
}

const fn sport(api_id: u32, sport_name: &'static str, rests: bool) -> Action {
    Action::Sport { api_id, sport_name, rests }
}

/// The robot's commands, in the order the UI shows them. Each is its own endpoint: POST api/drive/<name>.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "stand",
        label: "Stand",
        description: "Stand up and balance, ready to walk (StandUp, wait 3 s, BalanceStand); api/drive/move needs this first",
        action: Action::Stand,
    },
    Command { name: "lie_down", label: "Lie down", description: "Lie down (StandDown)", action: sport(1005, "StandDown", true) },
    Command {
        name: "pose",
        label: "Pose",
        description: "Balance-stand in pose mode: api/drive/move then tilts/turns the body in place instead of walking",
        action: Action::Pose,
    },
    Command {
        name: "sit",
        label: "Sit",
        description: "Sit down (the robot can't walk until it stands again)",
        action: sport(1009, "Sit", true),
    },
    Command {
        name: "jump",
        label: "Jump",
        description: "Jump forward (FrontJump) — needs clear space in front",
        action: sport(1031, "FrontJump", false),
    },
    Command { name: "hello", label: "Hello", description: "Wave a front paw", action: sport(1016, "Hello", false) },
    Command { name: "stretch", label: "Stretch", description: "Stretch", action: sport(1017, "Stretch", false) },
    Command {
        name: "recover",
        label: "Recover",
        description: "Get back on its feet after a fall (RecoveryStand)",
        action: sport(1006, "RecoveryStand", false),
    },
    Command { name: "relax", label: "Relax", description: "Go limp and lie down (Damp)", action: sport(SPORT_DAMP, "Damp", true) },
    Command { name: "rise_sit", label: "Rise", description: "Get up from sitting (RiseSit)", action: sport(1010, "RiseSit", false) },
    Command { name: "dance1", label: "Dance 1", description: "Dance routine 1", action: sport(1022, "Dance1", false) },
    Command { name: "dance2", label: "Dance 2", description: "Dance routine 2", action: sport(1023, "Dance2", false) },
    Command { name: "wiggle_hips", label: "Wiggle", description: "Wiggle its hips", action: sport(1033, "WiggleHips", false) },
    Command {
        name: "finger_heart",
        label: "Heart",
        description: "Make a heart with its front paws",
        action: sport(1036, "FingerHeart", false),
    },
];

pub fn commands_json() -> Value {
    json!(COMMANDS.iter().map(|c| json!({ "name": c.name, "label": c.label, "description": c.description })).collect::<Vec<_>>())
}

fn sends_for(command: &Command) -> Vec<Value> {
    match command.action {
        Action::Stand => vec![
            json!({ "motionMode": "normal" }),
            json!({ "sport": "StandUp", "apiId": SPORT_STAND_UP }),
            json!({ "waitMs": STAND_SETTLE.as_millis() as u64 }),
            json!({ "sport": "BalanceStand", "apiId": SPORT_BALANCE_STAND }),
        ],
        Action::Pose => vec![
            json!({ "sport": "BalanceStand", "apiId": SPORT_BALANCE_STAND }),
            json!({ "sport": "Pose", "apiId": SPORT_POSE, "parameter": { "data": true } }),
        ],
        Action::Sport { api_id, sport_name, .. } => vec![json!({ "sport": sport_name, "apiId": api_id })],
    }
}

struct DriveState {
    /// connecting | ready | reconnecting | error
    status: &'static str,
    error: Option<String>,
    /// resting | rising | stand | pose
    mode: &'static str,
    velocity: (f64, f64, f64),
    run: bool,
    move_until: Option<Instant>,
    moving: bool,
    last_command: Option<Value>,
}

pub struct Drive {
    pub robot: Option<String>,
    pub ip: String,
    pub name: String,
    pub dry: bool,
    aes_keys: Vec<crate::robot_rtc::AesKey>,
    started_at: u64,
    state: Mutex<DriveState>,
    conn: tokio::sync::Mutex<Option<Arc<RobotConn>>>,
    video: Mutex<Option<Arc<TrackLocalStaticRTP>>>,
    viewers: tokio::sync::Mutex<Vec<Arc<RTCPeerConnection>>>,
    closed: AtomicBool,
    app: Weak<App>,
}

pub fn no_session() -> Value {
    json!({ "active": false })
}

impl Drive {
    pub fn snapshot(&self) -> Value {
        let state = self.state.lock().unwrap();
        let (forward, strafe, turn) = state.velocity;
        json!({
            "active": true,
            "robot": self.robot,
            "ip": self.ip,
            "name": self.name,
            "dryRun": self.dry,
            "status": state.status,
            "error": state.error,
            "mode": state.mode,
            "velocity": { "forward": forward, "strafe": strafe, "turn": turn, "run": state.run },
            "moving": state.moving,
            "video": self.video.lock().unwrap().is_some(),
            "lastCommand": state.last_command,
            "startedAt": self.started_at,
        })
    }

    fn publish(&self) {
        if let Some(app) = self.app.upgrade() {
            app.publish(json!({ "type": "drive", "drive": self.snapshot() }));
        }
    }

    fn set(&self, update: impl FnOnce(&mut DriveState)) {
        update(&mut self.state.lock().unwrap());
        self.publish();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Opens a session (closing any other) and returns once it's ready — or failed, with the reason.
    pub async fn open(
        app: &Arc<App>,
        robot: Option<String>,
        ip: String,
        name: String,
        aes_key: Option<String>,
        dry_run: bool,
    ) -> Result<Value, HttpError> {
        let given = match aes_key.filter(|k| !k.is_empty()) {
            Some(key) => {
                crate::robot_rtc::parse_aes_key(&key).map_err(HttpError::bad)?;
                Some(("the request".to_string(), key.to_lowercase()))
            }
            None => {
                let saved = robot.as_deref().map(|key| (format!("saved for {key}"), app.aes_key_for(key))).filter(|(_, k)| !k.is_empty());
                // on a dog's hotspot, the key saved for that hotspot
                match saved {
                    None if ip == crate::hotspot::AP_IP => app
                        .wifi
                        .current()
                        .await
                        .map(|ssid| (format!("saved for hotspot {ssid}"), app.aes_key_for(&ssid)))
                        .filter(|(_, k)| !k.is_empty()),
                    saved => saved,
                }
            }
        };
        // the robot's own key first, then every other known one (a manual IP, or the dog's hotspot, names no robot)
        let mut aes_keys: Vec<crate::robot_rtc::AesKey> = given.into_iter().collect();
        for candidate in app.known_aes_keys() {
            if !aes_keys.iter().any(|(_, k)| *k == candidate.1) {
                aes_keys.push(candidate);
            }
        }
        crate::dlog!("drive open: robot {robot:?} ip {ip} dry {} ({} aes keys known)", dry_run || app.mock, aes_keys.len());
        close(app).await;
        let dry = dry_run || app.mock;
        let drive = Arc::new(Drive {
            robot,
            ip,
            name,
            dry,
            aes_keys,
            started_at: now_ms(),
            state: Mutex::new(DriveState {
                status: if dry { "ready" } else { "connecting" },
                error: None,
                mode: "resting",
                velocity: (0.0, 0.0, 0.0),
                run: false,
                move_until: None,
                moving: false,
                last_command: None,
            }),
            conn: tokio::sync::Mutex::new(None),
            video: Mutex::new(dry.then(crate::video::placeholder_track)),
            viewers: tokio::sync::Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            app: Arc::downgrade(app),
        });
        *app.drive.lock().await = Some(drive.clone());
        drive.publish();
        drive.clone().start_ticker();
        if dry {
            app.record_start().await?;
            return Ok(drive.snapshot());
        }
        match drive.clone().connect_until(Instant::now() + CONNECT_WINDOW).await {
            Ok(()) => {
                app.record_start().await?;
                Ok(drive.snapshot())
            }
            Err(err) => {
                drive.set(|s| {
                    s.status = "error";
                    s.error = Some(err.clone());
                });
                Err(HttpError::upstream(err))
            }
        }
    }

    // boxed: it and reconnect() spawn each other, so the future's type must be named to be Send
    fn connect_until(
        self: Arc<Self>,
        deadline: Instant,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
        Box::pin(async move {
            loop {
                let weak = Arc::downgrade(&self);
                let on_event: crate::robot_rtc::OnEvent = Arc::new(move |event| {
                    let Some(drive) = weak.upgrade() else { return };
                    match event {
                        ConnEvent::Video(track) => {
                            *drive.video.lock().unwrap() = Some(track);
                            drive.publish();
                        }
                        ConnEvent::Lost => {
                            tokio::spawn(drive.reconnect());
                        }
                        ConnEvent::Rtp(packet) => {
                            if let Some(active) = drive.app.upgrade().and_then(|app| app.recording()) {
                                active.on_rtp(packet);
                            }
                        }
                        ConnEvent::Data(message) => {
                            if let Some(active) = drive.app.upgrade().and_then(|app| app.recording()) {
                                active.on_data(&message);
                            }
                        }
                    }
                });
                match RobotConn::connect(&self.ip, &self.aes_keys, on_event).await {
                    Ok(conn) => {
                        self.remember_key(&conn);
                        if self.is_closed() {
                            conn.close().await;
                            return Err("the session was closed".into());
                        }
                        *self.conn.lock().await = Some(conn);
                        self.set(|s| {
                            s.status = "ready";
                            s.error = None;
                        });
                        // a recording that outlived a reconnect: subscribe the new link too
                        if self.app.upgrade().is_some_and(|app| app.recording().is_some()) {
                            self.start_streams().await;
                        }
                        return Ok(());
                    }
                    // an AES problem won't fix itself by retrying
                    Err(err) if err.contains("AES") || err.contains("busy") => {
                        crate::dlog!("connect {} failed, not retrying: {err}", self.ip);
                        return Err(err);
                    }
                    Err(err) if Instant::now() >= deadline || self.is_closed() => {
                        crate::dlog!("connect {} failed, giving up: {err}", self.ip);
                        return Err(err);
                    }
                    Err(err) => {
                        crate::dlog!("connect {} failed, retrying: {err}", self.ip);
                        tokio::time::sleep(Duration::from_millis(500)).await
                    }
                }
            }
        })
    }

    /// A robot that needed a key it had none saved for keeps the one it accepted, for next time.
    fn remember_key(&self, conn: &RobotConn) {
        let (Some(robot), Some(key), Some(app)) = (&self.robot, &conn.key_used, self.app.upgrade()) else { return };
        if app.aes_key_for(robot) != *key {
            crate::dlog!("saving the accepted aes key for {robot}");
            let _ = app.set_aes_key(robot, key);
        }
    }

    /// A dropped peer link (dog off Wi-Fi, track died) rebuilds the connection in place until the session is closed.
    async fn reconnect(self: Arc<Self>) {
        if self.is_closed() || self.state.lock().unwrap().status == "reconnecting" {
            return;
        }
        crate::dlog!("connection to {} lost: reconnecting", self.ip);
        self.set(|s| {
            s.status = "reconnecting";
            s.mode = "resting";
            s.move_until = None;
            s.moving = false;
        });
        *self.video.lock().unwrap() = None;
        if let Some(conn) = self.conn.lock().await.take() {
            conn.close().await;
        }
        while !self.is_closed() {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if self.clone().connect_until(Instant::now()).await.is_ok() {
                return;
            }
        }
    }

    fn start_ticker(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(DRIVE_TICK).await;
                let Some(drive) = weak.upgrade() else { break };
                if drive.is_closed() {
                    break;
                }
                enum Tick {
                    Move(f64, f64, f64),
                    Stop,
                    Idle,
                }
                let tick = {
                    let mut state = drive.state.lock().unwrap();
                    match state.move_until {
                        Some(until) if Instant::now() < until => {
                            state.moving = true;
                            let mult = if state.run { RUN_MULT } else { 1.0 };
                            let (f, s, t) = state.velocity;
                            Tick::Move(f * MAX_FORWARD * mult, s * MAX_LATERAL * mult, t * MAX_YAW * mult)
                        }
                        _ if state.moving => {
                            state.moving = false;
                            state.move_until = None;
                            state.velocity = (0.0, 0.0, 0.0);
                            Tick::Stop
                        }
                        _ => Tick::Idle,
                    }
                };
                let conn = if drive.dry { None } else { drive.conn.lock().await.clone() };
                if let Some(active) = drive.app.upgrade().and_then(|app| app.recording()) {
                    match tick {
                        Tick::Move(x, y, z) => active.cmd_vel(x, y, z),
                        Tick::Stop => active.cmd_vel(0.0, 0.0, 0.0),
                        Tick::Idle => {}
                    }
                }
                match tick {
                    Tick::Move(x, y, z) => {
                        if let Some(conn) = conn {
                            conn.sport(SPORT_MOVE, Some(json!({ "x": x, "y": y, "z": z }))).await;
                        }
                    }
                    Tick::Stop => {
                        if let Some(conn) = conn {
                            conn.sport(SPORT_STOP_MOVE, None).await;
                        }
                        drive.publish();
                    }
                    Tick::Idle => {}
                }
            }
        });
    }

    fn ready(&self) -> Result<(), HttpError> {
        let state = self.state.lock().unwrap();
        if state.status != "ready" {
            return Err(HttpError::conflict(format!(
                "the connection to {} is {}{}",
                self.name,
                state.status,
                state.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
            )));
        }
        Ok(())
    }

    async fn conn(&self) -> Result<Arc<RobotConn>, HttpError> {
        self.conn.lock().await.clone().ok_or_else(|| HttpError::conflict("not connected to the robot"))
    }

    pub async fn command(&self, command: &Command, dry_run: bool) -> Result<Value, HttpError> {
        crate::dlog!("command {} (dry {dry_run}, mode {})", command.name, self.state.lock().unwrap().mode);
        if let Err(err) = self.ready() {
            crate::dlog!("command {} refused: {}", command.name, err.message);
            return Err(err);
        }
        let sends = sends_for(command);
        let dry = dry_run || self.dry;
        if dry_run && !self.dry {
            // a dry run on a live session: say what would be sent, change nothing
            return Ok(json!({ "dryRun": true, "sent": false, "command": command.name, "robot": self.robot, "sends": sends }));
        }
        let record = json!({ "name": command.name, "label": command.label, "at": now_ms(), "sent": !dry, "dryRun": dry });
        self.set(|s| s.last_command = Some(record.clone()));
        if let Some(active) = self.app.upgrade().and_then(|app| app.recording()) {
            active.command(&json!({ "name": command.name, "sends": sends, "sent": !dry, "dryRun": dry }));
        }
        if let Some(app) = self.app.upgrade() {
            app.publish(json!({ "type": "command", "command": record }));
        }
        let conn = if dry { None } else { Some(self.conn().await?) };
        match command.action {
            Action::Stand => {
                if self.state.lock().unwrap().mode == "pose" {
                    if let Some(conn) = &conn {
                        conn.sport(SPORT_BALANCE_STAND, None).await; // already up: just drop pose
                    }
                } else {
                    self.set(|s| s.mode = "rising");
                    if let Some(conn) = &conn {
                        conn.set_motion_mode("normal").await;
                        conn.sport(SPORT_STAND_UP, None).await;
                    }
                    tokio::time::sleep(STAND_SETTLE).await;
                    if self.is_closed() {
                        return Err(HttpError::conflict("the session was closed while standing up"));
                    }
                    if let Some(conn) = &conn {
                        conn.sport(SPORT_BALANCE_STAND, None).await;
                    }
                }
                self.set(|s| s.mode = "stand");
            }
            Action::Pose => {
                if let Some(conn) = &conn {
                    conn.sport(SPORT_BALANCE_STAND, None).await;
                    conn.sport(SPORT_POSE, Some(json!({ "data": true }))).await;
                }
                self.set(|s| s.mode = "pose");
            }
            Action::Sport { api_id, rests, .. } => {
                if let Some(conn) = &conn {
                    conn.sport(api_id, None).await;
                }
                if rests {
                    self.set(|s| {
                        s.mode = "resting";
                        s.move_until = None;
                    });
                }
            }
        }
        Ok(
            json!({ "dryRun": dry, "sent": !dry, "command": command.name, "robot": self.robot, "sends": sends, "mode": self.state.lock().unwrap().mode }),
        )
    }

    /// Drives at a normalized velocity for `duration` (re-sent every 120 ms, then a stop), replacing any earlier move.
    pub fn drive(&self, forward: f64, strafe: f64, turn: f64, run: bool, duration: Duration, dry_run: bool) -> Result<Value, HttpError> {
        crate::dlog!("move forward {forward} strafe {strafe} turn {turn} run {run} for {duration:?} (dry {dry_run})");
        if let Err(err) = self.ready() {
            crate::dlog!("move refused: {}", err.message);
            return Err(err);
        }
        let mode = self.state.lock().unwrap().mode;
        if mode != "stand" && mode != "pose" {
            crate::dlog!("move refused: the robot isn't standing (mode {mode})");
            return Err(HttpError::conflict(format!("the robot isn't standing (mode: {mode}) — POST api/drive/stand first")));
        }
        let mult = if run { RUN_MULT } else { 1.0 };
        let mut reply = json!({
            "dryRun": dry_run || self.dry,
            "sent": !(dry_run || self.dry),
            "metersPerSecond": { "forward": forward * MAX_FORWARD * mult, "strafe": strafe * MAX_LATERAL * mult },
            "radiansPerSecond": turn * MAX_YAW * mult,
            "durationMs": duration.as_millis() as u64,
        });
        if dry_run && !self.dry {
            return Ok(reply);
        }
        self.set(|s| {
            s.velocity = (forward, strafe, turn);
            s.run = run;
            s.move_until = Some(Instant::now() + duration);
        });
        reply["mode"] = json!(mode);
        Ok(reply)
    }

    pub async fn stop(&self, dry_run: bool) -> Result<Value, HttpError> {
        crate::dlog!("stop (dry {dry_run})");
        self.ready()?;
        if dry_run && !self.dry {
            return Ok(json!({ "dryRun": true, "sent": false, "sends": [{ "sport": "StopMove", "apiId": SPORT_STOP_MOVE }] }));
        }
        self.set(|s| {
            s.move_until = None;
            s.moving = false;
            s.velocity = (0.0, 0.0, 0.0);
        });
        if let Some(active) = self.app.upgrade().and_then(|app| app.recording()) {
            active.command(&json!({ "name": "stop", "sends": [{ "sport": "StopMove", "apiId": SPORT_STOP_MOVE }], "sent": !self.dry, "dryRun": self.dry }));
            active.cmd_vel(0.0, 0.0, 0.0);
        }
        if !self.dry {
            self.conn().await?.sport(SPORT_STOP_MOVE, None).await;
        }
        Ok(json!({ "dryRun": self.dry, "sent": !self.dry, "stopped": true }))
    }

    /// The safe way down: stop moving, then StandDown (lie down, motors still holding: never Damp, which drops a
    /// standing dog). For a low battery or a bad link, without closing the app.
    pub async fn sit_down(&self, dry_run: bool) -> Result<Value, HttpError> {
        self.ready()?;
        let sends =
            vec![json!({ "sport": "StopMove", "apiId": SPORT_STOP_MOVE }), json!({ "sport": "StandDown", "apiId": SPORT_STAND_DOWN })];
        if dry_run && !self.dry {
            return Ok(json!({ "dryRun": true, "sent": false, "sends": sends }));
        }
        let record = json!({ "name": "sit_down", "label": "Sit down", "at": now_ms(), "sent": !self.dry, "dryRun": self.dry });
        self.set(|s| {
            s.move_until = None;
            s.moving = false;
            s.velocity = (0.0, 0.0, 0.0);
            s.mode = "resting";
            s.last_command = Some(record.clone());
        });
        if let Some(app) = self.app.upgrade() {
            app.publish(json!({ "type": "command", "command": record }));
            if let Some(active) = app.recording() {
                active.cmd_vel(0.0, 0.0, 0.0);
                active.command(&json!({ "name": "sit_down", "sends": sends, "sent": !self.dry, "dryRun": self.dry }));
            }
        }
        if !self.dry {
            let conn = self.conn().await?;
            conn.sport(SPORT_STOP_MOVE, None).await;
            conn.sport(SPORT_STAND_DOWN, None).await;
        }
        Ok(json!({ "dryRun": self.dry, "sent": !self.dry, "sends": sends, "mode": "resting" }))
    }

    /// While recording: the sensor topics (sensors.rs), traffic saving off (else the robot holds the lidar back), and a
    /// keyframe for the camera decoder. Read-only for the robot.
    pub async fn start_streams(&self) {
        let Some(conn) = self.conn.lock().await.clone() else { return };
        conn.set_traffic_saving(false).await;
        conn.lidar_on().await;
        for topic in crate::sensors::TOPICS {
            conn.subscribe(topic).await;
        }
        conn.request_keyframe().await;
    }

    pub async fn stop_streams(&self) {
        let Some(conn) = self.conn.lock().await.clone() else { return };
        for topic in crate::sensors::TOPICS {
            conn.unsubscribe(topic).await;
        }
    }

    pub async fn keyframe(&self) {
        if let Some(conn) = self.conn.lock().await.clone() {
            conn.request_keyframe().await;
        }
    }

    /// The velocity being commanded now (m/s, m/s, rad/s), zero when not moving (the mock robot follows it).
    pub fn commanded_velocity(&self) -> (f64, f64, f64) {
        let state = self.state.lock().unwrap();
        match state.move_until {
            Some(until) if Instant::now() < until => {
                let mult = if state.run { RUN_MULT } else { 1.0 };
                let (f, s, t) = state.velocity;
                (f * MAX_FORWARD * mult, s * MAX_LATERAL * mult, t * MAX_YAW * mult)
            }
            _ => (0.0, 0.0, 0.0),
        }
    }

    /// A page's WebRTC offer for the camera → the answer that streams it.
    pub async fn video(&self, offer_sdp: String) -> Result<Value, HttpError> {
        let candidates = offer_sdp.lines().filter(|l| l.starts_with("a=candidate")).count();
        let h264 = offer_sdp.contains("H264");
        crate::dlog!("viewer offer: {candidates} candidates, H264 {h264}");
        let track = self.video.lock().unwrap().clone().ok_or_else(|| {
            crate::dlog!("viewer refused: no video from the robot yet");
            HttpError::conflict("no video from the robot yet — try again in a second")
        })?;
        let (sdp, pc) = crate::video::answer_viewer(track, offer_sdp).await.map_err(|err| {
            crate::dlog!("viewer answer failed: {err}");
            HttpError::bad(err)
        })?;
        crate::dlog!("viewer answer: {} candidates", sdp.lines().filter(|l| l.starts_with("a=candidate")).count());
        {
            let mut viewers = self.viewers.lock().await;
            viewers.retain(|viewer| {
                viewer.connection_state() != webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed
            });
            viewers.push(pc);
        }
        if let Ok(conn) = self.conn().await {
            tokio::spawn(async move {
                for _ in 0..3 {
                    conn.request_keyframe().await;
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            });
        }
        Ok(json!({ "type": "answer", "sdp": sdp, "placeholder": self.dry }))
    }

    async fn shutdown(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(conn) = self.conn.lock().await.take() {
            if self.state.lock().unwrap().moving {
                conn.sport(SPORT_STOP_MOVE, None).await;
            }
            conn.close().await;
        }
        for viewer in self.viewers.lock().await.drain(..) {
            let _ = viewer.close().await;
        }
    }
}

/// Ends the current session, if any.
pub async fn close(app: &Arc<App>) -> Option<Value> {
    // a recording ends with its session (finished, so the file is complete)
    if app.recording().is_some() {
        let _ = app.record_stop().await;
    }
    let drive = app.drive.lock().await.take()?;
    let last = drive.snapshot();
    drive.shutdown().await;
    app.publish(json!({ "type": "drive", "drive": no_session() }));
    Some(last)
}
