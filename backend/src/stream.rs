// The camera over a WebSocket (GET api/drive/video.ws), for pages whose WebRTC video won't connect: the robot's H.264,
// reassembled from its RTP packets into access units (Annex B, unchanged, no re-encode), one binary message each:
// byte 0 = 1 for a keyframe, bytes 1..9 = microseconds since the stream started (little-endian), then the H.264. The
// page decodes it with WebCodecs. Assembled only while a socket is open; each new socket asks the robot for a keyframe.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc};
use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packet::Packet;

const QUEUE: usize = 4096;
const FANOUT: usize = 256;

pub struct VideoStream {
    packets: mpsc::Sender<Packet>,
    frames: broadcast::Sender<Arc<Vec<u8>>>,
}

impl VideoStream {
    pub fn start() -> VideoStream {
        let (packets, mut receiver) = mpsc::channel::<Packet>(QUEUE);
        let (frames, _) = broadcast::channel(FANOUT);
        let out = frames.clone();
        tokio::spawn(async move {
            let mut builder = SampleBuilder::new(64, H264Packet::default(), 90_000);
            let started = Instant::now();
            let mut sent = 0u64;
            while let Some(packet) = receiver.recv().await {
                if out.receiver_count() == 0 {
                    continue;
                }
                builder.push(packet);
                while let Some(sample) = builder.pop() {
                    let key = crate::camera::has_keyframe(&sample.data);
                    let mut message = Vec::with_capacity(sample.data.len() + 9);
                    message.push(key as u8);
                    message.extend_from_slice(&(started.elapsed().as_micros() as u64).to_le_bytes());
                    message.extend_from_slice(&sample.data);
                    let _ = out.send(Arc::new(message));
                    sent += 1;
                    if sent == 1 {
                        crate::dlog!("video.ws: first frame sent ({} bytes, keyframe {key})", sample.data.len());
                    }
                }
            }
        });
        VideoStream { packets, frames }
    }

    /// A packet from the robot (dropped when the assembler is behind).
    pub fn push(&self, packet: Packet) {
        let _ = self.packets.try_send(packet);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Vec<u8>>> {
        self.frames.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_camera_packets_come_out_as_annex_b_frames() {
        let stream = VideoStream::start();
        let mut frames = stream.subscribe();
        let mut camera = crate::camera::MockCamera::new().expect("openh264 encoder");
        let frame = loop {
            for packet in camera.next_packets("stream test") {
                stream.push(packet);
            }
            if let Ok(Ok(frame)) = tokio::time::timeout(std::time::Duration::from_millis(200), frames.recv()).await {
                break frame;
            }
        };
        assert_eq!(frame[0], 1, "the first frame is a keyframe");
        assert_eq!(&frame[9..13], &[0, 0, 0, 1], "Annex B start code after the 9-byte header");
    }
}
