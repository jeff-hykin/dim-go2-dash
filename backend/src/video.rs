// The robot's camera for the pages: each page opens its own receive-only WebRTC peer with this backend (POST
// api/drive/video with its SDP offer) and gets the robot's H.264 track forwarded packet for packet. The robot only
// allows one WebRTC client, so the backend holds that one and fans the video out.

use std::sync::Arc;
use std::time::Duration;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::TrackLocal;

/// Answers a page's offer with a peer that sends `track`. Returns the answer SDP and the peer (closed with the session).
pub async fn answer_viewer(track: Arc<TrackLocalStaticRTP>, offer_sdp: String) -> Result<(String, Arc<RTCPeerConnection>), String> {
    let mut media = MediaEngine::default();
    media.register_default_codecs().map_err(|e| e.to_string())?;
    let registry = register_default_interceptors(Registry::new(), &mut media).map_err(|e| e.to_string())?;
    let mut settings = SettingEngine::default();
    // the page usually runs on this same machine, possibly with no LAN at all
    settings.set_include_loopback_candidate(true);
    let api = APIBuilder::new().with_media_engine(media).with_interceptor_registry(registry).with_setting_engine(settings).build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.map_err(|e| e.to_string())?);
    let result = negotiate(&pc, track, offer_sdp).await;
    match result {
        Ok(sdp) => Ok((sdp, pc)),
        Err(err) => {
            let _ = pc.close().await;
            Err(err)
        }
    }
}

async fn negotiate(pc: &Arc<RTCPeerConnection>, track: Arc<TrackLocalStaticRTP>, offer_sdp: String) -> Result<String, String> {
    let sender = pc.add_track(track as Arc<dyn TrackLocal + Send + Sync>).await.map_err(|e| e.to_string())?;
    // drain RTCP so the interceptors (NACK, reports) keep working
    tokio::spawn(async move {
        let mut buffer = vec![0u8; 1500];
        while sender.read(&mut buffer).await.is_ok() {}
    });
    {
        let weak = Arc::downgrade(pc);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let weak = weak.clone();
            Box::pin(async move {
                if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                    if let Some(pc) = weak.upgrade() {
                        let _ = pc.close().await;
                    }
                }
            })
        }));
    }
    let offer = RTCSessionDescription::offer(offer_sdp).map_err(|e| format!("the offer isn't valid SDP: {e}"))?;
    pc.set_remote_description(offer).await.map_err(|e| format!("the offer isn't usable: {e}"))?;
    let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
    let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
    Ok(pc.local_description().await.ok_or("no local description")?.sdp)
}

/// A stand-in camera track for dry-run and mock sessions: negotiates like the real one but carries no frames.
pub fn placeholder_track() -> Arc<TrackLocalStaticRTP> {
    use webrtc::api::media_engine::MIME_TYPE_H264;
    use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
    Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        },
        "video".into(),
        "go2".into(),
    ))
}
