// The robot's camera for the pages: each page opens its own receive-only WebRTC peer with this backend (POST
// api/drive/video with its SDP offer) and gets the robot's H.264 track forwarded packet for packet. The robot only
// allows one WebRTC client, so the backend holds that one and fans the video out.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// the most often a page's picture-loss reports ask the robot for a keyframe
const KEYFRAME_EVERY: Duration = Duration::from_millis(500);

/// Answers a page's offer with a peer that sends `track`. Returns the answer SDP and the peer (closed with the session).
/// `want_keyframe` runs when the page reports a lost picture (PLI / FIR), at most every KEYFRAME_EVERY.
pub async fn answer_viewer(
    track: Arc<TrackLocalStaticRTP>,
    offer_sdp: String,
    want_keyframe: Arc<dyn Fn() + Send + Sync>,
) -> Result<(String, Arc<RTCPeerConnection>), String> {
    let mut media = MediaEngine::default();
    media.register_default_codecs().map_err(|e| e.to_string())?;
    let registry = register_default_interceptors(Registry::new(), &mut media).map_err(|e| e.to_string())?;
    let mut settings = SettingEngine::default();
    // the page usually runs on this same machine, possibly with no LAN at all
    settings.set_include_loopback_candidate(true);
    let api = APIBuilder::new().with_media_engine(media).with_interceptor_registry(registry).with_setting_engine(settings).build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await.map_err(|e| e.to_string())?);
    let result = negotiate(&pc, track, offer_sdp, want_keyframe).await;
    match result {
        Ok(sdp) => Ok((sdp, pc)),
        Err(err) => {
            let _ = pc.close().await;
            Err(err)
        }
    }
}

async fn negotiate(
    pc: &Arc<RTCPeerConnection>,
    track: Arc<TrackLocalStaticRTP>,
    offer_sdp: String,
    want_keyframe: Arc<dyn Fn() + Send + Sync>,
) -> Result<String, String> {
    let sender = pc.add_track(track as Arc<dyn TrackLocal + Send + Sync>).await.map_err(|e| e.to_string())?;
    // read RTCP so the interceptors (NACK, reports) keep working, and pass the page's picture-loss reports on
    tokio::spawn(async move {
        let mut last_request: Option<Instant> = None;
        while let Ok((packets, _)) = sender.read_rtcp().await {
            if asks_for_keyframe(&packets) && last_request.is_none_or(|at| at.elapsed() >= KEYFRAME_EVERY) {
                last_request = Some(Instant::now());
                want_keyframe();
            }
        }
    });
    {
        let weak = Arc::downgrade(pc);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            crate::dlog!("viewer peer state: {state}");
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

/// Whether a page's RTCP reports a lost picture (it can't decode until the next keyframe).
fn asks_for_keyframe(packets: &[Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>]) -> bool {
    use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
    use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
    packets.iter().any(|packet| packet.as_any().is::<PictureLossIndication>() || packet.as_any().is::<FullIntraRequest>())
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

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
    use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
    use webrtc::rtcp::receiver_report::ReceiverReport;

    #[test]
    fn a_pages_picture_loss_asks_for_a_keyframe_its_reports_do_not() {
        let pli: Box<dyn webrtc::rtcp::packet::Packet + Send + Sync> = Box::new(PictureLossIndication { sender_ssrc: 1, media_ssrc: 2 });
        let fir: Box<dyn webrtc::rtcp::packet::Packet + Send + Sync> = Box::new(FullIntraRequest::default());
        let report: Box<dyn webrtc::rtcp::packet::Packet + Send + Sync> = Box::new(ReceiverReport::default());
        assert!(asks_for_keyframe(&[pli]));
        assert!(asks_for_keyframe(&[fir]));
        assert!(!asks_for_keyframe(&[report]));
    }
}
