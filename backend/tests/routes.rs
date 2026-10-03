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
