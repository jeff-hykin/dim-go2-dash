// The camera as JPEG frames for pages that can't play the WebRTC video (GET api/drive/camera.jpg): Firefox decodes
// WebRTC H.264 only as Constrained Baseline, and the robot's stream isn't always that. The robot's H.264 RTP packets →
// frames (openh264) → the latest JPEG, on its own thread, and only while a page is fetching frames (a frame request
// keeps it on for WATCH_FOR); idle, it drops packets and resyncs on a keyframe when watched again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packet::Packet;

use crate::record::now_ns;

const QUEUE: usize = 2048;
const JPEG_QUALITY: u8 = 70;
const MAX_FPS: u64 = 12;
/// a frame request keeps decoding on this long
const WATCH_FOR_NS: u64 = 3_000_000_000;

pub struct Preview {
    sender: SyncSender<Packet>,
    latest: Arc<Mutex<Option<Arc<Vec<u8>>>>>,
    watched_until: Arc<AtomicU64>,
}

impl Preview {
    /// `want_keyframe` is called when decoding (re)starts and needs one.
    pub fn start(want_keyframe: Arc<dyn Fn() + Send + Sync>) -> Preview {
        let (sender, receiver) = sync_channel(QUEUE);
        let latest = Arc::new(Mutex::new(None));
        let watched_until = Arc::new(AtomicU64::new(0));
        {
            let (latest, watched_until) = (latest.clone(), watched_until.clone());
            std::thread::Builder::new()
                .name("go2-preview".into())
                .spawn(move || run(receiver, latest, watched_until, want_keyframe))
                .expect("spawn the preview thread");
        }
        Preview { sender, latest, watched_until }
    }

    /// A packet from the robot (dropped when the thread is behind).
    pub fn push(&self, packet: Packet) {
        let _ = self.sender.try_send(packet);
    }

    /// The newest frame, and keep decoding for a while (a page is watching).
    pub fn frame(&self) -> Option<Arc<Vec<u8>>> {
        self.watched_until.store(now_ns() + WATCH_FOR_NS, Ordering::Relaxed);
        self.latest.lock().unwrap().clone()
    }
}

fn has_keyframe(annex_b: &[u8]) -> bool {
    // NAL types after each start code: 5 = IDR slice, 7 = SPS
    annex_b.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && matches!(w[3] & 0x1f, 5 | 7))
}

fn run(
    receiver: Receiver<Packet>,
    latest: Arc<Mutex<Option<Arc<Vec<u8>>>>>,
    watched_until: Arc<AtomicU64>,
    want_keyframe: Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
        crate::dlog!("preview: openh264 didn't start; no camera.jpg");
        return;
    };
    let mut builder = SampleBuilder::new(64, H264Packet::default(), 90_000);
    let mut synced = false;
    let mut watching = false;
    let mut last_frame_ns = 0u64;
    let mut frames = 0u64;
    let mut rgb = Vec::new();
    while let Ok(packet) = receiver.recv() {
        let now = now_ns();
        let watched = now < watched_until.load(Ordering::Relaxed);
        if !watched {
            if watching {
                watching = false;
                synced = false;
                *latest.lock().unwrap() = None;
            }
            continue;
        }
        if !watching {
            watching = true;
            crate::dlog!("preview: a page is watching; decoding");
            want_keyframe();
        }
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
                Err(err) => {
                    crate::dlog!("preview: decode failed ({err}); waiting for a keyframe");
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
            let mut jpeg = Vec::with_capacity(width * height / 4);
            if jpeg_encoder::Encoder::new(&mut jpeg, JPEG_QUALITY)
                .encode(&rgb, width as u16, height as u16, jpeg_encoder::ColorType::Rgb)
                .is_err()
            {
                continue;
            }
            frames += 1;
            if frames == 1 {
                crate::dlog!("preview: first frame {width}x{height}");
            }
            *latest.lock().unwrap() = Some(Arc::new(jpeg));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_camera_packets_become_a_jpeg_while_watched() {
        let preview = Preview::start(Arc::new(|| {}));
        assert!(preview.frame().is_none(), "no frame before any video");
        let mut camera = crate::camera::MockCamera::new().expect("openh264 encoder");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let jpeg = loop {
            for packet in camera.next_packets("preview test") {
                preview.push(packet);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Some(jpeg) = preview.frame() {
                break jpeg;
            }
            assert!(std::time::Instant::now() < deadline, "no JPEG within 10 s");
        };
        assert_eq!(&jpeg[..2], &[0xff, 0xd8], "a JPEG (SOI marker)");
    }
}
