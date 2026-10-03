// A WebRTC session with one Go2, the same one the Unitree app opens: the signaling handshake over the robot's HTTP port
// (con_notify → RSA/AES-wrapped SDP offer → con_ing_*), then a data channel for validation, heartbeat and sport
// requests, and a receive-only H.264 camera track that video.rs passes on to the pages. Port of the browser client the
// panel used to run (unitree_go2_webrtc_js) and of dimos' unitree_webrtc_connect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rand::Rng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use webrtc::api::interceptor_registry::{configure_nack, configure_rtcp_reports};
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocalWriter;

const SIGNALING_PORT: u16 = 9991;
const SIGNALING_TIMEOUT: Duration = Duration::from_secs(4);
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(8);
const SPORT_TOPIC: &str = "rt/api/sport/request";
const MOTION_SWITCHER_TOPIC: &str = "rt/api/motion_switcher/request";
/// data2=2 firmware wraps con_notify in AES-128-GCM under this fixed key; data2=3 (≥ 1.1.15) uses a per-device key.
const LEGACY_GCM_KEY: [u8; 16] = [232, 86, 130, 189, 22, 84, 155, 0, 142, 4, 166, 104, 43, 179, 235, 227];

/// What a live connection tells its owner.
pub enum ConnEvent {
    /// the robot's camera, re-published for pages to subscribe to
    Video(Arc<TrackLocalStaticRTP>),
    /// the peer link died (robot off Wi-Fi, out of range, rebooted)
    Lost,
}

pub type OnEvent = Arc<dyn Fn(ConnEvent) + Send + Sync>;

pub struct RobotConn {
    pc: Arc<RTCPeerConnection>,
    channel: Arc<RTCDataChannel>,
    video_ssrc: Arc<Mutex<Option<u32>>>,
    closed: Arc<AtomicBool>,
}

// ── signaling crypto ──

fn gcm_decrypt(data1_b64: &str, key: &[u8; 16]) -> Result<String, String> {
    let raw = B64.decode(data1_b64.trim()).map_err(|e| format!("con_notify data1 isn't base64: {e}"))?;
    if raw.len() < 28 {
        return Err("con_notify data1 is too short".into());
    }
    let (rest, tag) = raw.split_at(raw.len() - 16);
    let (ciphertext, nonce) = rest.split_at(rest.len() - 12);
    let mut combined = ciphertext.to_vec();
    combined.extend_from_slice(tag);
    let cipher = Aes128Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let plain = cipher.decrypt(Nonce::from_slice(nonce), combined.as_slice()).map_err(|_| "GCM tag check failed".to_string())?;
    String::from_utf8(plain).map_err(|e| e.to_string())
}

pub fn parse_aes_key(hex: &str) -> Result<[u8; 16], String> {
    let clean = hex.trim().to_lowercase();
    if clean.len() != 32 || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("aesKey must be 32 hex characters (16 bytes)".into());
    }
    let mut key = [0u8; 16];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).unwrap();
    }
    Ok(key)
}

fn decrypt_data1(data1: &str, data2: i64, aes_key: &str) -> Result<String, String> {
    match data2 {
        2 => gcm_decrypt(data1, &LEGACY_GCM_KEY),
        3 => {
            if aes_key.is_empty() {
                return Err("This robot speaks data2=3 — a per-device AES-128 key is required (pull it from a Unitree account, or set it on the robot).".into());
            }
            gcm_decrypt(data1, &parse_aes_key(aes_key)?)
                .map_err(|_| "AES-128 key rejected by the robot (GCM tag check failed).".to_string())
        }
        _ => Ok(data1.to_string()),
    }
}

/// The con_ing path suffix: digits from the last 10 chars of data1 (every second char, as an index into A..J).
pub fn path_ending(data1: &str) -> String {
    let chars: Vec<char> = data1.chars().collect();
    let last10 = &chars[chars.len().saturating_sub(10)..];
    let mut out = String::new();
    for pair in last10.chunks(2) {
        if pair.len() > 1 {
            if let Some(index) = "ABCDEFGHIJ".find(pair[1]) {
                out.push_str(&index.to_string());
            }
        }
    }
    out
}

/// AES-256-ECB/PKCS7 under a 32-char key used as UTF-8 bytes, base64 (what crypto-js did for the browser client).
pub fn aes_ecb_encrypt(plain: &str, key: &str) -> String {
    let cipher = ecb::Encryptor::<aes::Aes256>::new_from_slice(key.as_bytes()).expect("32-byte key");
    B64.encode(cipher.encrypt_padded_vec_mut::<aes::cipher::block_padding::Pkcs7>(plain.as_bytes()))
}

pub fn aes_ecb_decrypt(b64: &str, key: &str) -> Result<String, String> {
    let raw = B64.decode(b64.trim()).map_err(|e| format!("answer isn't base64: {e}"))?;
    let cipher = ecb::Decryptor::<aes::Aes256>::new_from_slice(key.as_bytes()).expect("32-byte key");
    let plain = cipher
        .decrypt_padded_vec_mut::<aes::cipher::block_padding::Pkcs7>(&raw)
        .map_err(|_| "couldn't decrypt the robot's answer".to_string())?;
    String::from_utf8(plain).map_err(|e| e.to_string())
}

fn rsa_encrypt(data: &str, public_key_b64_der: &str) -> Result<String, String> {
    let der = B64.decode(public_key_b64_der.trim()).map_err(|e| format!("robot public key isn't base64: {e}"))?;
    let key = RsaPublicKey::from_public_key_der(&der).map_err(|e| format!("robot public key: {e}"))?;
    let encrypted = key.encrypt(&mut rand::thread_rng(), Pkcs1v15Encrypt, data.as_bytes()).map_err(|e| e.to_string())?;
    Ok(B64.encode(encrypted))
}

pub fn validation_reply(key: &str) -> String {
    B64.encode(md5::compute(format!("UnitreeGo2_{key}")).0)
}

async fn signal(ip: &str, path: &str, body: Option<String>) -> Result<String, String> {
    let client = reqwest::Client::builder().timeout(SIGNALING_TIMEOUT).build().map_err(|e| e.to_string())?;
    let mut request = client.post(format!("http://{ip}:{SIGNALING_PORT}{path}")).header("content-type", "application/json");
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request.send().await.map_err(|e| format!("robot {ip} unreachable: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("signaling {path} failed ({})", response.status()));
    }
    response.text().await.map_err(|e| e.to_string())
}

/// Hands the robot our SDP offer and returns its answer (JSON `{sdp, type}`).
async fn exchange_sdp(ip: &str, aes_key: &str, offer_json: &str) -> Result<Value, String> {
    let notify_b64 = signal(ip, "/con_notify", None).await?;
    let notify_raw = B64.decode(notify_b64.trim()).map_err(|e| format!("con_notify isn't base64: {e}"))?;
    let notify: Value = serde_json::from_slice(&notify_raw).map_err(|e| format!("con_notify isn't JSON: {e}"))?;
    let data1 = notify["data1"].as_str().ok_or("con_notify has no data1")?;
    let data2 = notify["data2"].as_i64().unwrap_or(0);
    let data1 = decrypt_data1(data1, data2, aes_key)?;
    if data1.len() < 20 {
        return Err("con_notify data1 is too short".into());
    }
    let public_key = &data1[10..data1.len() - 10];
    let session_key: String = (0..16).map(|_| format!("{:02x}", rand::thread_rng().gen::<u8>())).collect();
    let body = json!({ "data1": aes_ecb_encrypt(offer_json, &session_key), "data2": rsa_encrypt(&session_key, public_key)? });
    let answer = signal(ip, &format!("/con_ing_{}", path_ending(&data1)), Some(body.to_string())).await?;
    let answer = aes_ecb_decrypt(&answer, &session_key)?;
    serde_json::from_str(&answer).map_err(|e| format!("answer isn't JSON: {e}"))
}

async fn send(channel: &RTCDataChannel, kind: &str, topic: &str, data: Option<Value>) {
    let mut message = json!({ "type": kind, "topic": topic });
    if let Some(data) = data {
        message["data"] = data;
    }
    let _ = channel.send_text(message.to_string()).await;
}

fn robot_api() -> Result<webrtc::api::API, String> {
    let mut media = MediaEngine::default();
    media.register_default_codecs().map_err(|e| e.to_string())?;
    // no transport-cc: the robot rejects offers that ask for it
    let mut registry = Registry::new();
    registry = configure_nack(registry, &mut media);
    registry = configure_rtcp_reports(registry);
    Ok(APIBuilder::new().with_media_engine(media).with_interceptor_registry(registry).build())
}

impl RobotConn {
    /// Opens the session and returns once the robot has validated the data channel (it's then ready for commands).
    pub async fn connect(ip: &str, aes_key: &str, on_event: OnEvent) -> Result<Arc<RobotConn>, String> {
        let pc = Arc::new(robot_api()?.new_peer_connection(RTCConfiguration::default()).await.map_err(|e| e.to_string())?);
        let result = Self::negotiate(pc.clone(), ip, aes_key, on_event).await;
        if result.is_err() {
            let _ = pc.close().await;
        }
        result
    }

    async fn negotiate(pc: Arc<RTCPeerConnection>, ip: &str, aes_key: &str, on_event: OnEvent) -> Result<Arc<RobotConn>, String> {
        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
        )
        .await
        .map_err(|e| e.to_string())?;
        let channel = pc.create_data_channel("data", None).await.map_err(|e| e.to_string())?;
        let closed = Arc::new(AtomicBool::new(false));
        let video_ssrc = Arc::new(Mutex::new(None));

        // validation: the robot sends a key, we answer md5("UnitreeGo2_" + key); "Validation Ok." means ready
        let (validated_tx, validated_rx) = oneshot::channel::<()>();
        let validated_tx = Arc::new(std::sync::Mutex::new(Some(validated_tx)));
        let last_key = Arc::new(std::sync::Mutex::new(String::new()));
        {
            let channel_for_reply = channel.clone();
            channel.on_message(Box::new(move |message: DataChannelMessage| {
                let channel = channel_for_reply.clone();
                let validated_tx = validated_tx.clone();
                let last_key = last_key.clone();
                Box::pin(async move {
                    if !message.is_string {
                        return;
                    }
                    let Ok(message) = serde_json::from_slice::<Value>(&message.data) else {
                        return;
                    };
                    let kind = message["type"].as_str().unwrap_or("");
                    if kind == "validation" {
                        let data = message["data"].as_str().unwrap_or("").to_string();
                        if data == "Validation Ok." {
                            if let Some(tx) = validated_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                        } else {
                            *last_key.lock().unwrap() = data.clone();
                            send(&channel, "validation", "", Some(json!(validation_reply(&data)))).await;
                        }
                    } else if kind == "err" && message["info"] == "Validation Needed." {
                        let key = last_key.lock().unwrap().clone();
                        send(&channel, "validation", "", Some(json!(validation_reply(&key)))).await;
                    }
                })
            }));
        }

        // camera: re-publish each RTP packet on a local track pages can subscribe to (video.rs)
        {
            let on_event = on_event.clone();
            let closed = closed.clone();
            let video_ssrc = video_ssrc.clone();
            pc.on_track(Box::new(move |track, _receiver, _transceiver| {
                let on_event = on_event.clone();
                let closed = closed.clone();
                let video_ssrc = video_ssrc.clone();
                Box::pin(async move {
                    if track.kind() != RTPCodecType::Video {
                        return;
                    }
                    *video_ssrc.lock().await = Some(track.ssrc());
                    let local = Arc::new(TrackLocalStaticRTP::new(track.codec().capability.clone(), "video".into(), "go2".into()));
                    on_event(ConnEvent::Video(local.clone()));
                    tokio::spawn(async move {
                        while let Ok((packet, _)) = track.read_rtp().await {
                            if closed.load(Ordering::Relaxed) {
                                break;
                            }
                            let _ = local.write_rtp(&packet).await;
                        }
                    });
                })
            }));
        }
        {
            let on_event = on_event.clone();
            let closed = closed.clone();
            pc.on_peer_connection_state_change(Box::new(move |state| {
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected)
                    && !closed.swap(true, Ordering::Relaxed)
                {
                    on_event(ConnEvent::Lost);
                }
                Box::pin(async {})
            }));
        }

        let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
        let mut gathered = pc.gathering_complete_promise().await;
        pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
        let _ = tokio::time::timeout(Duration::from_secs(4), gathered.recv()).await;
        let local = pc.local_description().await.ok_or("no local description")?;
        let offer_json = json!({ "id": "STA_localNetwork", "sdp": local.sdp, "type": "offer", "token": "" }).to_string();

        let answer = exchange_sdp(ip, aes_key, &offer_json).await?;
        let sdp = answer["sdp"].as_str().unwrap_or("").to_string();
        if sdp == "reject" {
            return Err("Robot is busy — another WebRTC client is connected.".into());
        }
        let answer = RTCSessionDescription::answer(sdp).map_err(|e| e.to_string())?;
        pc.set_remote_description(answer).await.map_err(|e| e.to_string())?;

        match tokio::time::timeout(VALIDATION_TIMEOUT, validated_rx).await {
            Ok(Ok(())) => {}
            _ => return Err("the robot never validated the data channel".into()),
        }
        let conn = Arc::new(RobotConn { pc, channel, video_ssrc, closed });
        conn.start_heartbeat();
        send(&conn.channel, "vid", "", Some(json!("on"))).await;
        Ok(conn)
    }

    fn start_heartbeat(self: &Arc<Self>) {
        let conn = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(conn) = conn.upgrade() else { break };
                if conn.closed.load(Ordering::Relaxed) {
                    break;
                }
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                let text = format_utc(now);
                send(&conn.channel, "heartbeat", "", Some(json!({ "timeInStr": text, "timeInNum": now }))).await;
            }
        });
    }

    async fn request(&self, topic: &str, api_id: u32, parameter: Option<Value>) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let id = now % 2_147_483_648 + rand::thread_rng().gen_range(0..1000);
        let parameter = match parameter {
            None => json!(""),
            Some(Value::String(text)) => json!(text),
            Some(value) => json!(value.to_string()),
        };
        let payload = json!({ "header": { "identity": { "id": id, "api_id": api_id } }, "parameter": parameter });
        send(&self.channel, "req", topic, Some(payload)).await;
    }

    pub async fn sport(&self, api_id: u32, parameter: Option<Value>) {
        self.request(SPORT_TOPIC, api_id, parameter).await;
    }

    pub async fn set_motion_mode(&self, name: &str) {
        self.request(MOTION_SWITCHER_TOPIC, 1002, Some(json!({ "name": name }))).await;
    }

    /// Ask the robot for a fresh keyframe, so a page that just subscribed doesn't wait for the next one.
    pub async fn request_keyframe(&self) {
        if let Some(ssrc) = *self.video_ssrc.lock().await {
            let _ = self.pc.write_rtcp(&[Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc })]).await;
        }
    }

    pub async fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.pc.close().await;
    }
}

/// "YYYY-MM-DD HH:MM:SS" (UTC) for the heartbeat, without a date crate.
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_roundtrip_and_path_ending() {
        let key = "0123456789abcdef0123456789abcdef";
        assert_eq!(aes_ecb_decrypt(&aes_ecb_encrypt("hello go2", key), key).unwrap(), "hello go2");
        // pairs "xA" "xB" "xC" "xD" "xJ" → 0 1 2 3 9
        assert_eq!(path_ending("....xAxBxCxDxJ"), "01239");
    }

    #[test]
    fn utc_format_and_validation() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21 14:13:20");
        assert_eq!(validation_reply("abc").len(), 24);
    }

    #[tokio::test]
    async fn robot_offer_has_no_transport_cc() {
        let pc = robot_api().unwrap().new_peer_connection(RTCConfiguration::default()).await.unwrap();
        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, send_encodings: vec![] }),
        )
        .await
        .unwrap();
        pc.create_data_channel("data", None).await.unwrap();
        let offer = pc.create_offer(None).await.unwrap().sdp;
        assert!(offer.contains("H264") && offer.contains("webrtc-datachannel"));
        assert!(!offer.contains("transport-cc") && !offer.contains("transport-wide-cc"), "{offer}");
        pc.close().await.unwrap();
    }

    #[test]
    fn rsa_wraps_the_session_key_like_jsencrypt() {
        use rsa::pkcs8::EncodePublicKey;
        let private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
        let der = private.to_public_key().to_public_key_der().unwrap();
        let wrapped = rsa_encrypt("0123456789abcdef0123456789abcdef", &B64.encode(der.as_bytes())).unwrap();
        let plain = private.decrypt(Pkcs1v15Encrypt, &B64.decode(wrapped).unwrap()).unwrap();
        assert_eq!(plain, b"0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn aes_key_parsing() {
        assert!(parse_aes_key("00112233445566778899aabbccddeeff").is_ok());
        assert!(parse_aes_key("xyz").is_err());
    }
}
