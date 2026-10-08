// Every route: a happy path and an error, against a mock app (GO2_DASH_MOCK's simulation: no Bluetooth, network,
// cloud or robot is touched). Robot-moving and Wi-Fi-changing routes are exercised with dryRun / dry sessions only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use go2_dash::app::App;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Test {
    app: Arc<App>,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup() -> Test {
    let dir = std::env::temp_dir().join(format!("go2_dash_test_{}_{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    Test { app: App::new(dir.clone(), true), _dir: TempDir(dir) }
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

impl Test {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let request = Request::builder()
            .method(method)
            .uri(format!("/{path}"))
            .header("content-type", "application/json")
            .body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty))
            .unwrap();
        let response = go2_dash::server::router(self.app.clone(), None).oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, json) = self.call(method, path, body).await;
        assert_eq!(status, 200, "{method} {path} → {json}");
        json
    }

    async fn status(&self, method: &str, path: &str, body: Option<Value>) -> u16 {
        self.call(method, path, body).await.0
    }

    async fn scanned(&self) {
        self.ok("POST", "api/scan", Some(json!({ "timeout": 1 }))).await;
    }
}

const DOG: &str = "MOCK0000GO2A0001";
const BLE_ONLY: &str = "MOCK0000GO2A0002";

#[tokio::test]
async fn state_and_agent_json() {
    let t = setup();
    let state = t.ok("GET", "api/state", None).await;
    assert_eq!(state["drive"]["active"], false);
    assert!(state["commands"].as_array().unwrap().iter().any(|c| c["name"] == "jump"));
    let agent = t.ok("GET", "agent.json", None).await;
    assert_eq!(agent["endpoints"].as_array().unwrap().len(), t.app.routes.len());
    assert!(agent["endpoints"].as_array().unwrap().iter().any(|e| e["path"] == "api/drive/jump" && e["method"] == "POST"));
    assert_eq!(t.status("GET", "api/nope", None).await, 404);
    assert_eq!(t.status("POST", "api/state", None).await, 404);
}

#[tokio::test]
async fn scan_and_robots() {
    let t = setup();
    assert_eq!(t.status("POST", "api/scan", Some(json!({ "timeout": 0 }))).await, 400);
    assert_eq!(t.status("POST", "api/scan", Some(json!({ "timeout": "x" }))).await, 400);
    let scan = t.ok("POST", "api/scan", Some(json!({ "timeout": 1 }))).await;
    assert_eq!(scan["robots"].as_array().unwrap().len(), 2);
    assert_eq!(scan["scan"]["scanning"], false);
    let robots = t.ok("GET", "api/robots", None).await;
    assert_eq!(robots[0]["key"], DOG);
    assert_eq!(robots[0]["ip"], "192.0.2.10");
    assert_eq!(t.ok("GET", &format!("api/robots/{DOG}"), None).await["serial"], DOG);
    assert_eq!(t.status("GET", "api/robots/nobody", None).await, 404);
    // a scan that doesn't wait answers at once, while it runs
    let started = t.ok("POST", "api/scan?timeout=1&wait=false", None).await;
    assert_eq!(started["scan"]["scanning"], true);
}

#[tokio::test]
async fn scan_sweep_match_and_stop() {
    let t = setup();
    assert_eq!(t.status("POST", "api/scan", Some(json!({ "timeout": 1, "sweep": "everything" }))).await, 400);
    let scan = t.ok("POST", "api/scan", Some(json!({ "timeout": 1, "sweep": "full" }))).await;
    assert_eq!(scan["scan"]["sweep"]["status"], "done");
    assert_eq!(scan["scan"]["sweep"]["swept"], "192.0.2.0/24");
    let robots = t.ok("GET", "api/robots", None).await;
    assert_eq!(robots[0]["matched"], "ble+arp");
    assert_eq!(robots[1]["matched"], "ble");
    // "use this IP": the guide's robot and its IP, nothing connected
    let picked = t.ok("PUT", "api/setup", Some(json!({ "robot": DOG, "ip": "192.0.2.10" }))).await;
    assert_eq!(picked["robot"]["key"], DOG);
    assert_eq!(picked["robot"]["ip"], "192.0.2.10");
    // nothing to stop once it's done; a running one stops, keeping what it found
    assert_eq!(t.ok("POST", "api/scan/stop", None).await["stopped"], false);
    t.ok("POST", "api/scan?timeout=5&wait=false", None).await;
    assert_eq!(t.ok("POST", "api/scan/stop", None).await["stopped"], true);
    let mut done = false;
    for _ in 0..50 {
        let scan = t.ok("GET", "api/state", None).await["scan"].clone();
        if scan["scanning"] == false {
            assert_eq!(scan["sweep"]["status"], "cancelled");
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(done, "the scan didn't stop");
}

#[tokio::test]
async fn rename_aes_key_and_ip() {
    let t = setup();
    t.scanned().await;
    let renamed = t.ok("PUT", &format!("api/robots/{DOG}/name"), Some(json!({ "name": "Rex" }))).await;
    assert_eq!(renamed["name"], "Rex");
    assert_eq!(t.status("PUT", &format!("api/robots/{DOG}/name"), None).await, 400);
    assert_eq!(t.status("PUT", "api/robots/nobody/name", Some(json!({ "name": "x" }))).await, 404);
    // the name persists for the next app run
    assert_eq!(App::new(t._dir.0.clone(), true).routes.len(), t.app.routes.len());

    let keyed = t.ok("PUT", &format!("api/robots/{DOG}/aes-key"), Some(json!({ "aesKey": "00112233445566778899AABBCCDDEEFF" }))).await;
    assert_eq!(keyed["hasAesKey"], true);
    let export = t.ok("GET", "api/aes-keys", None).await;
    assert_eq!(export["keys"][0]["serial"], DOG);
    assert_eq!(export["keys"][0]["name"], "Rex");
    assert_eq!(export["keys"][0]["aesKey"].as_str().unwrap().to_lowercase(), "00112233445566778899aabbccddeeff");
    assert_eq!(t.status("PUT", &format!("api/robots/{DOG}/aes-key"), Some(json!({ "aesKey": "nothex" }))).await, 400);

    let ip = t.ok("PUT", &format!("api/robots/{BLE_ONLY}/ip"), Some(json!({ "ip": "192.0.2.20" }))).await;
    assert_eq!(ip["ip"], "192.0.2.20");
    assert_eq!(ip["ipSource"], "remembered");
    assert_eq!(t.status("PUT", &format!("api/robots/{BLE_ONLY}/ip"), Some(json!({ "ip": "999.1.1.1" }))).await, 400);
}

#[tokio::test]
async fn network() {
    let t = setup();
    let network = t.ok("GET", "api/network", None).await;
    assert_eq!(network["ssid"], "MockNet");
    assert_eq!(network["mock"], true);
}

#[tokio::test]
async fn wifi_dry_run_cancel_and_clear() {
    let t = setup();
    t.scanned().await;
    let path = format!("api/robots/{DOG}/wifi");
    assert_eq!(t.status("POST", &path, Some(json!({ "dryRun": true }))).await, 400); // no ssid
    assert_eq!(t.status("POST", &path, Some(json!({ "ssid": "Home", "country": "USA", "dryRun": true }))).await, 400);
    assert_eq!(t.status("POST", "api/robots/nobody/wifi", Some(json!({ "ssid": "Home", "dryRun": true }))).await, 404);
    let dry = t.ok("POST", &path, Some(json!({ "ssid": "Home", "password": "secret", "dryRun": true }))).await;
    assert_eq!(dry["dryRun"], true);
    assert_eq!(dry["sent"], false);
    assert!(dry["warning"].as_str().unwrap().contains("MockNet"));
    assert!(!dry.to_string().contains("secret"), "the password is never echoed");
    let wifi = t.ok("GET", "api/wifi", None).await;
    assert_eq!(wifi["status"], "dry-run");
    assert_eq!(t.status("POST", "api/wifi/cancel", None).await, 409); // nothing running
    assert_eq!(t.ok("DELETE", "api/wifi", None).await["status"], "idle");
}

#[tokio::test]
async fn drive_dry_session_commands_move_stop_video() {
    let t = setup();
    t.scanned().await;
    // with no session: dry runs say so, real ones are refused
    assert_eq!(t.status("POST", "api/drive/jump", None).await, 409);
    assert_eq!(t.ok("POST", "api/drive/jump", Some(json!({ "dryRun": true }))).await["sent"], false);
    assert_eq!(t.status("POST", "api/drive/move", Some(json!({ "forward": 1 }))).await, 409);
    assert_eq!(t.status("POST", "api/drive/stop", None).await, 409);
    assert_eq!(t.status("POST", "api/drive/disconnect", None).await, 409);
    assert_eq!(t.status("POST", "api/drive/video", Some(json!({ "sdp": "v=0" }))).await, 409);
    assert_eq!(t.ok("GET", "api/drive", None).await["active"], false);

    assert_eq!(t.status("POST", "api/drive/connect", Some(json!({ "dryRun": true }))).await, 400);
    assert_eq!(t.status("POST", "api/drive/connect", Some(json!({ "robot": BLE_ONLY, "dryRun": true }))).await, 409); // no IP
    assert_eq!(t.status("POST", "api/drive/connect", Some(json!({ "ip": "not-an-ip", "dryRun": true }))).await, 400);
    let session = t.ok("POST", "api/drive/connect", Some(json!({ "robot": DOG, "dryRun": true }))).await;
    assert_eq!(session["status"], "ready");
    assert_eq!(session["dryRun"], true);
    assert_eq!(t.ok("GET", "api/drive", None).await["robot"], DOG);

    // moving needs a stand first
    assert_eq!(t.status("POST", "api/drive/move", Some(json!({ "forward": 1, "dryRun": true }))).await, 409);
    let stood = t.ok("POST", "api/drive/stand", Some(json!({ "dryRun": true }))).await;
    assert_eq!(stood["mode"], "stand");
    assert_eq!(stood["sent"], false);
    let jump = t.ok("POST", "api/drive/jump", Some(json!({ "dryRun": true }))).await;
    assert_eq!(jump["sends"][0]["apiId"], 1031);
    assert_eq!(t.ok("GET", "api/drive", None).await["lastCommand"]["name"], "jump");
    assert_eq!(t.status("POST", "api/drive/move", Some(json!({ "forward": 2 }))).await, 400);
    assert_eq!(t.status("POST", "api/drive/move", Some(json!({ "forward": 1, "durationMs": 10 }))).await, 400);
    let moved = t.ok("POST", "api/drive/move", Some(json!({ "forward": 1, "turn": -0.5, "durationMs": 300, "dryRun": true }))).await;
    assert_eq!(moved["metersPerSecond"]["forward"], 0.6);
    assert_eq!(t.ok("POST", "api/drive/stop", Some(json!({ "dryRun": true }))).await["stopped"], true);
    assert_eq!(t.status("POST", "api/drive/stop", Some(json!({ "dryRun": "maybe" }))).await, 400);
    t.ok("POST", "api/drive/sit", Some(json!({ "dryRun": true }))).await;
    assert_eq!(t.ok("GET", "api/drive", None).await["mode"], "resting");

    // the camera endpoint answers a real WebRTC offer
    assert_eq!(t.status("POST", "api/drive/video", Some(json!({ "sdp": "garbage" }))).await, 400);
    let answer = t.ok("POST", "api/drive/video", Some(json!({ "sdp": browser_offer().await }))).await;
    assert!(answer["sdp"].as_str().unwrap().starts_with("v=0"));

    assert_eq!(t.ok("POST", "api/drive/disconnect", None).await["closed"]["robot"], DOG);
    assert_eq!(t.ok("GET", "api/drive", None).await["active"], false);
}

#[tokio::test]
async fn every_command_has_a_dry_run() {
    let t = setup();
    t.ok("POST", "api/drive/connect", Some(json!({ "ip": "192.0.2.30", "dryRun": true }))).await;
    for command in go2_dash::drive::COMMANDS.iter().filter(|c| c.name != "stand") {
        let reply = t.ok("POST", &format!("api/drive/{}", command.name), Some(json!({ "dryRun": true }))).await;
        assert_eq!(reply["sent"], false, "{}", command.name);
    }
    assert_eq!(t.status("POST", "api/drive/hello", Some(json!({ "dryRun": 3 }))).await, 400);
}

#[tokio::test]
async fn accounts() {
    let t = setup();
    assert_eq!(t.status("POST", "api/accounts", Some(json!({ "email": "a@b.c" }))).await, 400);
    assert_eq!(t.status("POST", "api/accounts", Some(json!({ "email": "nope", "password": "x" }))).await, 400);
    let account = t.ok("POST", "api/accounts", Some(json!({ "email": "Me+go2@Example.com", "password": "pw" }))).await;
    assert_eq!(account["email"], "me+go2@example.com");
    assert_eq!(account["robots"][0]["hasKey"], true);
    assert_eq!(t.ok("GET", "api/accounts", None).await.as_array().unwrap().len(), 1);
    assert_eq!(t.ok("POST", "api/accounts/me+go2@example.com/pull", None).await["error"], Value::Null);
    assert_eq!(t.status("POST", "api/accounts/other@example.com/pull", None).await, 404);
    // the pulled key lands on the robot with that serial
    t.scanned().await;
    assert_eq!(t.ok("GET", &format!("api/robots/{DOG}"), None).await["hasAesKey"], true);
    assert_eq!(t.ok("DELETE", "api/accounts/me%2Bgo2%40example.com", None).await["removed"], "me+go2@example.com");
    assert_eq!(t.status("DELETE", "api/accounts/me+go2@example.com", None).await, 404);
}

/// A receive-only video offer, like the page's.
async fn browser_offer() -> String {
    use webrtc::api::media_engine::MediaEngine;
    use webrtc::api::APIBuilder;
    use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
    use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
    use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
    let mut media = MediaEngine::default();
    media.register_default_codecs().unwrap();
    let api = APIBuilder::new().with_media_engine(media).build();
    let pc = api.new_peer_connection(Default::default()).await.unwrap();
    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
    )
    .await
    .unwrap();
    let offer = pc.create_offer(None).await.unwrap();
    let sdp = offer.sdp.clone();
    pc.close().await.unwrap();
    sdp
}

#[tokio::test]
async fn first_run_guide_walks_and_resumes() {
    let t = setup();
    let setup = t.ok("GET", "api/setup", None).await;
    assert_eq!(setup["step"], "welcome", "nothing saved yet: a first run opens the guide");
    assert_eq!(t.ok("GET", "api/state", None).await["setup"]["step"], "welcome");
    assert_eq!(t.status("PUT", "api/setup", Some(json!({ "step": "nowhere" }))).await, 400);
    assert_eq!(t.status("PUT", "api/setup", Some(json!({ "mode": "boat" }))).await, 400);
    assert_eq!(t.status("PUT", "api/setup", Some(json!({ "robot": "nobody" }))).await, 404);
    t.ok("PUT", "api/setup", Some(json!({ "step": "find", "mode": "robot" }))).await;
    t.scanned().await;
    // the Bluetooth-only dog (a new Go2): picked, given Wi-Fi, then seen on the network
    let picked = t.ok("PUT", "api/setup", Some(json!({ "robot": BLE_ONLY, "step": "wifi" }))).await;
    assert_eq!(picked["robot"]["key"], BLE_ONLY);
    assert_eq!(picked["robot"]["ip"], Value::Null);
    let sent = t.ok("POST", &format!("api/robots/{BLE_ONLY}/wifi"), Some(json!({ "ssid": "MockNet", "password": "pw" }))).await;
    assert_eq!(sent["sent"], true);
    t.scanned().await;
    assert_eq!(t.ok("GET", &format!("api/robots/{BLE_ONLY}"), None).await["ip"], Value::Null, "not on the network yet");
    tokio::time::sleep(std::time::Duration::from_millis(go2_dash::app::MOCK_JOIN_MS + 100)).await;
    t.scanned().await;
    let joined = t.ok("GET", &format!("api/robots/{BLE_ONLY}"), None).await;
    assert_eq!(joined["ip"], "192.0.2.11");
    // the IP confirmed (or edited) on the address step is saved with the robot
    assert_eq!(t.status("PUT", "api/setup", Some(json!({ "ip": "300.1.1.1" }))).await, 400);
    let confirmed = t.ok("PUT", "api/setup", Some(json!({ "ip": "192.0.2.11", "step": "launch" }))).await;
    assert_eq!(confirmed["robot"]["ip"], "192.0.2.11");
    assert_eq!(confirmed["robot"]["hasAesKey"], false);
    // a reload, or the app restarting, resumes where it was
    let again = App::new(t._dir.0.clone(), true);
    assert_eq!(again.setup_state()["step"], "launch");
    assert_eq!(again.setup_state()["robot"]["key"], BLE_ONLY);
    assert_eq!(t.ok("DELETE", "api/setup", None).await["step"], "welcome");
}

#[tokio::test]
async fn guide_without_a_scan_and_for_returning_users() {
    let t = setup();
    // no robot picked: the IP typed in is the robot
    let typed = t.ok("PUT", "api/setup", Some(json!({ "ip": "192.0.2.50" }))).await;
    assert_eq!(typed["robot"]["key"], "192.0.2.50");
    assert_eq!(typed["robot"]["name"], "Go2 at 192.0.2.50");
    assert_eq!(t.ok("PUT", "api/setup", Some(json!({ "robot": "" }))).await["robot"], Value::Null);
    // someone with saved robots already isn't walked through it again
    let dir = std::env::temp_dir().join(format!("go2_dash_test_returning_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("go2_dash_names.json"), r#"{"X":"Rex"}"#).unwrap();
    assert_eq!(App::new(dir.clone(), true).setup_state()["step"], "done");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn check_ip() {
    let t = setup();
    assert_eq!(t.ok("POST", "api/check-ip", Some(json!({ "ip": "192.0.2.10" }))).await["reachable"], true);
    let away = t.ok("POST", "api/check-ip", Some(json!({ "ip": "10.9.9.9" }))).await;
    assert_eq!(away["reachable"], false);
    assert!(away["error"].is_string());
    assert_eq!(t.status("POST", "api/check-ip", Some(json!({ "ip": "dog" }))).await, 400);
}

#[tokio::test]
async fn launch_in_mock_and_dry_run() {
    let t = setup();
    // nothing to launch for yet
    assert_eq!(t.status("POST", "api/launch", Some(json!({ "dryRun": true }))).await, 400);
    assert_eq!(t.status("POST", "api/launch", Some(json!({ "blueprint": "--help", "replay": true, "dryRun": true }))).await, 400);
    t.scanned().await;
    t.ok("PUT", &format!("api/robots/{DOG}/aes-key"), Some(json!({ "aesKey": "00112233445566778899aabbccddeeff" }))).await;
    t.ok("PUT", "api/setup", Some(json!({ "robot": DOG }))).await;
    let dry = t.ok("POST", "api/launch", Some(json!({ "dryRun": true }))).await;
    assert_eq!(dry["sent"], false);
    assert_eq!(dry["request"]["blueprint"], "unitree-go2-basic");
    assert_eq!(dry["request"]["overrides"]["robot_ip"], "192.0.2.10");
    assert_eq!(dry["request"]["overrides"]["unitree_aes_128_key"], "(saved key)", "the key is sent, never shown");
    // what really goes to Desktop carries the key
    let request = t.app.launch_request(false, None, None).unwrap();
    assert_eq!(request["overrides"]["unitree_aes_128_key"], "00112233445566778899aabbccddeeff");
    assert_eq!(
        t.app.launch_request(true, None, None).unwrap(),
        json!({ "blueprint": "unitree-go2-basic", "replay": true, "overrides": {} })
    );
    // mock: a robot launch is simulated, step by step
    assert_eq!(t.ok("GET", "api/launch", None).await["launch"], Value::Null);
    let launch = t.ok("POST", "api/launch", Some(json!({}))).await;
    assert_eq!(launch["phase"], "starting");
    assert_eq!(launch["mock"], true);
    tokio::time::sleep(std::time::Duration::from_millis(3700)).await;
    assert_eq!(t.ok("GET", "api/launch", None).await["launch"]["phase"], "running");
    assert_eq!(t.ok("POST", "api/launch/stop", None).await["stopped"], true);
    assert_eq!(t.ok("GET", "api/launch", None).await["launch"], Value::Null);
    // a replay needs Desktop
    assert_eq!(t.status("POST", "api/launch", Some(json!({ "replay": true }))).await, 409);
}

/// A stand-in for Desktop's /dimos endpoints: records what it's sent.
async fn fake_desktop(busy: bool) -> (String, Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
    use axum::routing::{get, post};
    let seen: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Default::default();
    let (s1, s2, s3) = (seen.clone(), seen.clone(), seen.clone());
    let router = axum::Router::new()
        .route(
            "/dimos/runs",
            post(move |body: axum::Json<Value>| {
                let seen = s1.clone();
                async move {
                    seen.lock().unwrap().push(("launch".into(), body.0.clone()));
                    if busy {
                        return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({ "error": "unitree-go2 is still running; stop it first" })));
                    }
                    let flags = body.0["overrides"].as_object().unwrap().iter().map(|(k, v)| format!("--{} {}", k.replace('_', "-"), v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(" ");
                    (axum::http::StatusCode::OK, axum::Json(json!({ "blueprint": body.0["blueprint"], "phase": "starting", "overrides": body.0["overrides"], "output": format!("$ dimos {flags} run x"), "steps": [] })))
                }
            })
            .get(|| async { axum::Json(json!({ "runs": [], "launch": { "blueprint": "unitree-go2-basic", "phase": "running", "overrides": { "unitree_aes_128_key": "ff" }, "output": "ok" } })) }),
        )
        .route(
            "/dimos/runs/stop",
            post(move |body: String| {
                let seen = s2.clone();
                async move {
                    seen.lock().unwrap().push(("stop".into(), serde_json::from_str(&body).unwrap_or(Value::Null)));
                    axum::Json(json!({ "output": "" }))
                }
            }),
        )
        .route(
            "/dimos/global-config",
            get(|| async { axum::Json(json!({ "overrides": { "zenoh_mode": "peer" } })) }).put(move |body: axum::Json<Value>| {
                let seen = s3.clone();
                async move {
                    seen.lock().unwrap().push(("global-config".into(), body.0));
                    axum::Json(json!({}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn launch_through_desktop() {
    let dir = std::env::temp_dir().join(format!("go2_dash_test_desktop_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _cleanup = TempDir(dir.clone());
    // a real (not mock) app: nothing here scans or reaches a robot, it only talks to the fake Desktop
    let t = Test { app: App::new(dir.clone(), false), _dir: TempDir(std::env::temp_dir().join("go2_dash_unused")) };
    let (url, seen) = fake_desktop(false).await;
    t.app.desktop_url.set(url).unwrap();
    t.ok("PUT", "api/setup", Some(json!({ "ip": "10.0.0.7" }))).await;
    let launch = t.ok("POST", "api/launch", Some(json!({ "default": true }))).await;
    assert_eq!(launch["phase"], "starting");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "global-config");
        assert_eq!(seen[0].1["overrides"], json!({ "zenoh_mode": "peer", "robot_ip": "10.0.0.7" }), "Desktop's other settings are kept");
        assert_eq!(
            seen[1],
            ("launch".to_string(), json!({ "blueprint": "unitree-go2-basic", "replay": false, "overrides": { "robot_ip": "10.0.0.7" } }))
        );
    }
    let state = t.ok("GET", "api/launch", None).await["launch"].clone();
    assert_eq!(state["phase"], "running");
    assert_eq!(state["overrides"]["unitree_aes_128_key"], "(saved key)");
    t.ok("POST", "api/launch", Some(json!({ "replay": true, "blueprint": "unitree-go2" }))).await;
    assert_eq!(seen.lock().unwrap()[2].1, json!({ "blueprint": "unitree-go2", "replay": true, "overrides": {} }));
    t.ok("POST", "api/launch/stop", None).await;
    assert!(seen.lock().unwrap().iter().any(|(name, _)| name == "stop"));
    // Desktop already running something: a 409 that says so
    let (busy_url, _) = fake_desktop(true).await;
    let busy = Test { app: App::new(dir.join("busy"), false), _dir: TempDir(std::env::temp_dir().join("go2_dash_unused2")) };
    busy.app.desktop_url.set(busy_url).unwrap();
    let (status, body) = busy.call("POST", "api/launch", Some(json!({ "ip": "10.0.0.7" }))).await;
    assert_eq!(status, 409);
    assert!(body["error"].as_str().unwrap().contains("stop it first"));
}

#[test]
fn data_dir_and_migration() {
    use go2_dash::app::{data_dir, migrate_legacy};
    use std::path::PathBuf;
    let home = PathBuf::from("/home/u");
    assert_eq!(data_dir(None, None, home.clone(), false), PathBuf::from("/home/u/.local/share/dim"));
    assert_eq!(data_dir(None, Some("/d/app".into()), home.clone(), false), PathBuf::from("/d/app"));
    assert_eq!(data_dir(Some("/x".into()), Some("/d/app".into()), home.clone(), true), PathBuf::from("/x/mock"));
    let root = std::env::temp_dir().join(format!("go2_dash_test_migrate_{}", std::process::id()));
    let _cleanup = TempDir(root.clone());
    let (old, new) = (root.join("old"), root.join("new"));
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("go2_dash_names.json"), "{}").unwrap();
    assert_eq!(migrate_legacy(&old, &new), 1);
    std::fs::write(old.join("go2_dash_ips.json"), "{}").unwrap();
    assert_eq!(migrate_legacy(&old, &new), 0, "only once: the new folder already has saved files");
}
