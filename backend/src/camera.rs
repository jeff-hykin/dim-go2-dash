// The camera for recordings: the robot's H.264 RTP packets → frames (openh264) → JPEG → /color_image
// (sensor_msgs/CompressedImage, frame camera_optical), plus /camera_info once a second (dimos' Go2 front-camera
// calibration). Runs on its own thread with a bounded queue: when it falls behind it drops packets, waits for the next
// keyframe and asks the robot for one, so memory never grows.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packet::Packet;

use crate::cdr::{CameraInfo, Header};
use crate::msg::Msg;
use crate::record::{jpeg_quality, now_ns, Recorder};

const QUEUE: usize = 2048;
/// at most this many frames a second go into the file
const MAX_FPS: u64 = 15;

/// Feeds the camera thread; dropping it ends the thread.
pub struct CameraTap {
    sender: SyncSender<Packet>,
}

impl CameraTap {
    /// Returns false when the queue is full (the packet was dropped).
    pub fn push(&self, packet: Packet) -> bool {
        !matches!(self.sender.try_send(packet), Err(TrySendError::Full(_)))
    }
}

/// `want_keyframe` is called when the decoder needs a keyframe (start, after a drop).
pub fn start(recorder: Arc<Recorder>, want_keyframe: Arc<dyn Fn() + Send + Sync>) -> CameraTap {
    let (sender, receiver) = sync_channel(QUEUE);
    std::thread::Builder::new()
        .name("go2-camera".into())
        .spawn(move || run(receiver, recorder, want_keyframe))
        .expect("spawn the camera thread");
    CameraTap { sender }
}

/// dimos/robot/unitree/go2/front_camera_720.yaml
pub fn go2_camera_info() -> CameraInfo<'static> {
    const D: &[f64] = &[-0.07309428880537933, -0.02341140740909078, -0.0069305931780026956, 0.009238684474464793];
    let (fx, cx, fy, cy) = (797.4756164864929, 643.5352167821186, 796.4872112769983, 349.2783605343087);
    CameraInfo {
        width: 1280,
        height: 720,
        distortion_model: "equidistant",
        d: D,
        k: [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0],
        r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        p: [fx, 0.0, cx, 0.0, 0.0, fy, cy, 0.0, 0.0, 0.0, 1.0, 0.0],
    }
}

pub fn has_keyframe(annex_b: &[u8]) -> bool {
    // NAL types after each start code: 5 = IDR slice, 7 = SPS
    annex_b.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && matches!(w[3] & 0x1f, 5 | 7))
}

fn run(receiver: Receiver<Packet>, recorder: Arc<Recorder>, want_keyframe: Arc<dyn Fn() + Send + Sync>) {
    let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
        eprintln!("camera: openh264 didn't start; no /color_image in this recording");
        return;
    };
    let mut builder = SampleBuilder::new(64, H264Packet::default(), 90_000);
    let mut synced = false;
    let mut last_frame_ns = 0u64;
    let mut last_info_ns = 0u64;
    let mut rgb = Vec::new();
    want_keyframe();
    while let Ok(packet) = receiver.recv() {
        builder.push(packet);
        while let Some(sample) = builder.pop() {
            if !synced {
                if !has_keyframe(&sample.data) {
                    continue;
                }
                synced = true;
            }
            let decoded = match decoder.decode(&sample.data) {
                Ok(Some(yuv)) => yuv,
                Ok(None) => continue,
                Err(_) => {
                    synced = false;
                    want_keyframe();
                    continue;
                }
            };
            let now = now_ns();
            if now.saturating_sub(last_frame_ns) < 1_000_000_000 / MAX_FPS {
                continue;
            }
            last_frame_ns = now;
            use openh264::formats::YUVSource;
            let (width, height) = decoded.dimensions();
            rgb.resize(width * height * 3, 0);
            decoded.write_rgb8(&mut rgb);
            let header = Header { stamp_ns: now, frame_id: "camera_optical" };
            let image = match jpeg_quality(&recorder.image_format()) {
                None => Msg::Image { header, width: width as u32, height: height as u32, rgb: rgb.clone() },
                Some(quality) => {
                    let mut jpeg = Vec::with_capacity(width * height / 4);
                    let encoder = jpeg_encoder::Encoder::new(&mut jpeg, quality);
                    if encoder.encode(&rgb, width as u16, height as u16, jpeg_encoder::ColorType::Rgb).is_err() {
                        continue;
                    }
                    Msg::Jpeg { header, width: width as u32, height: height as u32, data: jpeg }
                }
            };
            recorder.write("/color_image", image);
            if now.saturating_sub(last_info_ns) >= 1_000_000_000 {
                last_info_ns = now;
                let mut info = go2_camera_info();
                // the calibration is for 1280×720: scale it to the stream's size
                let (sx, sy) = (width as f64 / info.width as f64, height as f64 / info.height as f64);
                for (i, s) in [(0, sx), (2, sx), (4, sy), (5, sy)] {
                    info.k[i] *= s;
                }
                for (i, s) in [(0, sx), (2, sx), (5, sy), (6, sy)] {
                    info.p[i] *= s;
                }
                info.width = width as u32;
                info.height = height as u32;
                recorder.write("/camera_info", Msg::CameraInfo(header, info));
            }
        }
    }
}

/// The mock robot's camera: a moving test pattern encoded to H.264 and packetized like the robot's RTP, so a mock
/// recording goes through exactly the path above.
pub struct MockCamera {
    encoder: openh264::encoder::Encoder,
    packetizer: Box<dyn webrtc::rtp::packetizer::Packetizer + Send + Sync>,
    frame: u32,
}

impl MockCamera {
    pub const WIDTH: usize = 640;
    pub const HEIGHT: usize = 360;

    pub fn new() -> Option<MockCamera> {
        let encoder = openh264::encoder::Encoder::new().ok()?;
        let packetizer = webrtc::rtp::packetizer::new_packetizer(
            1200,
            102,
            0x1234_5678,
            Box::new(webrtc::rtp::codecs::h264::H264Payloader::default()),
            Box::new(webrtc::rtp::sequence::new_random_sequencer()),
            90_000,
        );
        Some(MockCamera { encoder, packetizer: Box::new(packetizer), frame: 0 })
    }

    /// The next frame's RTP packets (call at the frame rate).
    pub fn next_packets(&mut self, label: &str) -> Vec<Packet> {
        let (w, h) = (Self::WIDTH, Self::HEIGHT);
        let t = self.frame as usize;
        self.frame += 1;
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = ((x + t * 4) % 256) as u8;
                rgb[i + 1] = ((y * 255) / h) as u8;
                rgb[i + 2] = if (x / 40 + y / 40 + t / 15) % 2 == 0 { 40 } else { 200 };
            }
        }
        // a white bar sweeping across, so motion is visible
        let bar = (t * 6) % w;
        for y in 0..h {
            for x in bar..(bar + 12).min(w) {
                let i = (y * w + x) * 3;
                rgb[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let _ = label;
        let yuv = openh264::formats::YUVBuffer::from_rgb8_source(openh264::formats::RgbSliceU8::new(&rgb, (w, h)));
        let Ok(stream) = self.encoder.encode(&yuv) else { return Vec::new() };
        let annex_b = stream.to_vec();
        self.packetizer.packetize(&bytes::Bytes::from(annex_b), 90_000 / 15).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_camera_round_trips_to_jpeg() {
        let dir = std::env::temp_dir().join(format!("go2_camera_test_{}", now_ns()));
        let path = dir.join("c.mcap");
        let options = crate::record::RecordOptions { format: "mcap".into(), ..Default::default() };
        let recorder = Arc::new(Recorder::start_with_options(&path, Default::default(), |_| Default::default(), options).unwrap());
        let tap = start(recorder.clone(), Arc::new(|| {}));
        let mut camera = MockCamera::new().unwrap();
        for _ in 0..30 {
            for packet in camera.next_packets("test") {
                assert!(tap.push(packet));
            }
            std::thread::sleep(std::time::Duration::from_millis(70));
        }
        drop(tap);
        std::thread::sleep(std::time::Duration::from_millis(300));
        recorder.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let images: Vec<_> =
            mcap::MessageStream::new(&bytes).unwrap().map(|m| m.unwrap()).filter(|m| m.channel.topic == "/color_image").collect();
        assert!(images.len() >= 10, "{} frames", images.len());
        assert_eq!(images[0].channel.schema.as_ref().unwrap().name, "sensor_msgs/msg/CompressedImage");
        // the JPEG starts after header + "jpeg" string + length: find its SOI marker
        assert!(images[0].data.windows(3).any(|w| w == [0xFF, 0xD8, 0xFF]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
