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
            let mut builder = SampleBuilder::new(crate::camera::MAX_LATE_PACKETS, H264Packet::default(), 90_000);
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

    #[tokio::test]
    async fn a_720p_keyframe_of_hundreds_of_packets_comes_out_whole() {
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, QpRange, RateControlMode};
        use webrtc::rtp::packetizer::Packetizer;
        let (width, height) = (1280usize, 720usize);
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(15.0))
            .intra_frame_period(IntraFramePeriod::from_num_frames(30))
            .rate_control_mode(RateControlMode::Off)
            .qp(QpRange::new(12, 16))
            .bitrate(BitRate::from_bps(20_000_000));
        let mut encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), config).unwrap();
        let mut packetizer = webrtc::rtp::packetizer::new_packetizer(
            1200,
            102,
            0x1234_5678,
            Box::new(webrtc::rtp::codecs::h264::H264Payloader::default()),
            Box::new(webrtc::rtp::sequence::new_random_sequencer()),
            90_000,
        );
        // noise: incompressible, so the keyframe runs to hundreds of packets like a real 720p one
        let noise: Vec<u8> = (0..width * height * 3).map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let stream = VideoStream::start();
        let mut frames = stream.subscribe();
        let mut keyframe = (0, 0);
        for n in 0..3 {
            let yuv = openh264::formats::YUVBuffer::from_rgb8_source(openh264::formats::RgbSliceU8::new(&noise, (width, height)));
            let annex_b = encoder.encode(&yuv).unwrap().to_vec();
            let packets = packetizer.packetize(&bytes::Bytes::from(annex_b.clone()), 90_000 / 15).unwrap();
            if n == 0 {
                keyframe = (annex_b.len(), packets.len());
            }
            for packet in packets {
                stream.push(packet);
                tokio::task::yield_now().await;
            }
        }
        assert!(keyframe.1 > 64, "the keyframe is only {} packets", keyframe.1);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), frames.recv()).await.expect("no frame came out").unwrap();
        assert_eq!(frame[0], 1, "the first frame is the keyframe");
        assert_eq!(frame.len() - 9, keyframe.0, "the keyframe came out truncated");
    }
}
