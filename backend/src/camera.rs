// The camera for recordings: the robot's H.264 RTP packets → frames (openh264) → JPEG → /color_image
// (sensor_msgs/CompressedImage, frame camera_optical), plus /camera_info once a second (dimos' Go2 front-camera
// calibration). Runs on its own thread with a bounded queue: when it falls behind it drops packets, waits for the next
// keyframe and asks the robot for one, so memory never grows. A frame is stamped with when the robot captured it: its
// RTP timestamp at the clock rate measured from the arrivals (CaptureClock), placed on the wall clock, not decode time.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use webrtc::media::io::sample_builder::SampleBuilder;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packet::Packet;

use crate::msg::{CameraInfo, Header, Msg};
use crate::record::{jpeg_quality, now_ns, Recorder};

const QUEUE: usize = 2048;
/// RTP packets the sample builder waits for before giving up on a frame. 720p keyframes can run to a few hundred
/// packets; at webrtc's default of 64 the builder emits such a frame truncated and the frames after it fail to
/// decode. Latency is unaffected: a frame is still released as soon as the next one's first packet arrives.
const MAX_LATE_PACKETS: u16 = 4096;

/// Feeds the camera thread; dropping it ends the thread once it has written every frame already queued.
pub struct CameraTap {
    sender: Option<SyncSender<(Packet, u64)>>,
    dropped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for CameraTap {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl CameraTap {
    /// Returns false when the queue is full (the packet was dropped; the camera thread then resyncs on a keyframe).
    pub fn push(&self, packet: Packet) -> bool {
        let Some(sender) = &self.sender else { return false };
        let full = matches!(sender.try_send((packet, now_ns())), Err(TrySendError::Full(_)));
        if full {
            self.dropped.store(true, Ordering::Relaxed);
        }
        !full
    }
}

/// Capture times for decoded frames: the decoder hands frames out in display order, which a stream with reordered
/// frames doesn't send them in, so a decoded frame takes the earliest RTP timestamp still waiting. The robot's RTP
/// clock isn't trusted to run at 90 kHz (the Go2's advances about 60 ticks a frame): its rate is measured from the
/// first frame's arrival to the latest frame's, and every stamp is placed from the first frame's arrival at that rate.
#[derive(Default)]
struct CaptureClock {
    /// RTP timestamps (unwrapped) of frames given to the decoder and not yet out of it
    waiting: BinaryHeap<Reverse<i64>>,
    last_rtp: Option<u32>,
    unwrapped: i64,
    /// the first frame: its ticks and when its first packet arrived (wall clock ns)
    first: Option<(i64, u64)>,
    /// the frame with the largest ticks so far and its arrival: a frame sent as soon as it was captured
    latest: Option<(i64, u64)>,
}

impl CaptureClock {
    fn reset(&mut self) {
        *self = CaptureClock::default();
    }

    fn unwrap(&mut self, rtp: u32) -> i64 {
        if let Some(last) = self.last_rtp {
            self.unwrapped += rtp.wrapping_sub(last) as i32 as i64;
        } else {
            self.unwrapped = rtp as i64;
        }
        self.last_rtp = Some(rtp);
        self.unwrapped
    }

    /// A frame went into the decoder: its RTP timestamp and when its first packet arrived.
    fn submitted(&mut self, rtp: u32, arrived_ns: u64) {
        let ticks = self.unwrap(rtp);
        self.first.get_or_insert((ticks, arrived_ns));
        if self.latest.is_none_or(|(t, _)| ticks > t) {
            self.latest = Some((ticks, arrived_ns));
        }
        self.waiting.push(Reverse(ticks));
        // a frame the decoder never hands out mustn't shift every later frame's time
        while self.waiting.len() > 16 {
            self.waiting.pop();
        }
    }

    /// Wall-clock nanoseconds per RTP tick, measured; 90 kHz until two frames have arrived.
    fn ns_per_tick(&self) -> f64 {
        match (self.first, self.latest) {
            (Some((t0, a0)), Some((t1, a1))) if t1 > t0 && a1 > a0 => (a1 - a0) as f64 / (t1 - t0) as f64,
            _ => 1e9 / 90_000.0,
        }
    }

    /// The capture time (wall clock ns) of the frame the decoder just handed out.
    fn decoded(&mut self) -> Option<u64> {
        let Reverse(ticks) = self.waiting.pop()?;
        let (t0, a0) = self.first?;
        Some((a0 as i64 + ((ticks - t0) as f64 * self.ns_per_tick()) as i64).max(0) as u64)
    }
}

/// `want_keyframe` is called when the decoder needs a keyframe (start, after a drop).
pub fn start(recorder: Arc<Recorder>, want_keyframe: Arc<dyn Fn() + Send + Sync>) -> CameraTap {
    let (sender, receiver) = sync_channel(QUEUE);
    let dropped = Arc::new(AtomicBool::new(false));
    let thread_dropped = dropped.clone();
    let thread = std::thread::Builder::new()
        .name("go2-camera".into())
        .spawn(move || run(receiver, thread_dropped, recorder, want_keyframe))
        .expect("spawn the camera thread");
    CameraTap { sender: Some(sender), dropped, thread: Some(thread) }
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

fn run(
    receiver: Receiver<(Packet, u64)>,
    dropped: Arc<AtomicBool>,
    recorder: Arc<Recorder>,
    want_keyframe: Arc<dyn Fn() + Send + Sync>,
) {
    // NoFlush: the crate's default flushes after every decode, which on a stream with reordered frames errors on all
    // but the frames right after a keyframe (recordings had ~1 fps: a keyframe, a frame, then nothing until the next)
    let config = openh264::decoder::DecoderConfig::new().flush_after_decode(openh264::decoder::Flush::NoFlush);
    let Ok(mut decoder) = openh264::decoder::Decoder::with_api_config(openh264::OpenH264API::from_source(), config) else {
        eprintln!("camera: openh264 didn't start; no /color_image in this recording");
        return;
    };
    let mut builder = SampleBuilder::new(MAX_LATE_PACKETS, H264Packet::default(), 90_000);
    let mut synced = false;
    let mut clock = CaptureClock::default();
    // when each RTP timestamp's first packet arrived (a frame's packets share one); a frame is popped after its last
    let mut first_arrival: std::collections::VecDeque<(u32, u64)> = Default::default();
    let mut last_info_ns = 0u64;
    let mut rgb = Vec::new();
    want_keyframe();
    while let Ok((packet, arrived_ns)) = receiver.recv() {
        if first_arrival.back().is_none_or(|&(rtp, _)| rtp != packet.header.timestamp) {
            first_arrival.push_back((packet.header.timestamp, arrived_ns));
            if first_arrival.len() > 64 {
                first_arrival.pop_front();
            }
        }
        builder.push(packet);
        while let Some(sample) = builder.pop() {
            // packets lost on the way, or dropped here when the queue was full: the frames after them can't decode
            if dropped.swap(false, Ordering::Relaxed) || sample.prev_dropped_packets > 0 {
                if synced {
                    synced = false;
                    want_keyframe();
                }
            }
            if !synced {
                if !has_keyframe(&sample.data) {
                    continue;
                }
                synced = true;
                clock.reset();
            }
            let arrived = first_arrival.iter().find(|&&(rtp, _)| rtp == sample.packet_timestamp).map_or_else(now_ns, |&(_, at)| at);
            clock.submitted(sample.packet_timestamp, arrived);
            let decoded = match decoder.decode(&sample.data) {
                Ok(Some(yuv)) => yuv,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("camera: decode failed ({error}); waiting for a keyframe");
                    synced = false;
                    want_keyframe();
                    continue;
                }
            };
            let now = clock.decoded().unwrap_or_else(now_ns);
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
    use webrtc::rtp::packetizer::Packetizer;

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
            mcap::MessageStream::new(&bytes).unwrap().map(|m| m.unwrap()).filter(|m| m.channel.topic == "color_image").collect();
        assert!(images.len() >= 10, "{} frames", images.len());
        assert_eq!(images[0].channel.message_encoding, "jpeg");
        let image = lcm_msgs::sensor_msgs::Image::decode(&images[0].data).unwrap();
        assert_eq!((image.encoding.as_str(), &image.data[..3]), ("jpeg", &[0xFF, 0xD8, 0xFF][..]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Frames go into the decoder in send order (I P B B …) and come out in display order: each takes the earliest
    /// capture time still waiting, placed by the packet that arrived soonest after its capture.
    #[test]
    fn capture_clock_stamps_reordered_frames_in_display_order() {
        let mut clock = CaptureClock::default();
        let start = u32::MAX - 6000; // wraps
        // send order with display index: 0, 3, 1, 2, 6, 4, 5; a frame can't be sent before the latest frame it follows
        // in send order was captured, and each arrives 50 ms after that (B frames wait longer)
        let sent = [0u32, 3, 1, 2, 6, 4, 5];
        let base_ns = 1_000_000_000_000u64;
        let mut out = Vec::new();
        let mut latest = 0;
        for (i, &frame) in sent.iter().enumerate() {
            latest = latest.max(frame);
            clock.submitted(start.wrapping_add(frame * 3000), base_ns + latest as u64 * 33_333_333 + 50_000_000);
            // the decoder holds one frame back for reordering
            if i >= 1 {
                out.push(clock.decoded().unwrap());
            }
        }
        out.push(clock.decoded().unwrap());
        // display order, 1/30 s apart, each at its capture time plus the 50 ms the quickest frames took
        for (i, &stamp) in out.iter().enumerate() {
            let expected = base_ns + i as u64 * 33_333_333 + 50_000_000;
            assert!(stamp.abs_diff(expected) < 10, "frame {i}: {stamp} vs {expected}");
        }
        assert!(clock.decoded().is_none());
    }

    /// A stream with reordered (B) frames, like the robot's: every frame has to come out, not just the keyframe's.
    /// The Go2's RTP timestamps advance about 60 ticks a frame, not 6000: read as 90 kHz, a minute of video
    /// collapsed into half a second. The clock takes its rate from the arrivals instead.
    #[test]
    fn capture_clock_measures_the_robot_clock_rate() {
        let mut clock = CaptureClock::default();
        let base_ns = 1_000_000_000_000u64;
        let mut out = Vec::new();
        for i in 0..30u64 {
            clock.submitted(1000 + i as u32 * 60, base_ns + i * 66_666_667);
            out.push(clock.decoded().unwrap());
        }
        for (i, &stamp) in out.iter().enumerate() {
            let expected = base_ns + i as u64 * 66_666_667;
            assert!(stamp.abs_diff(expected) < 1000, "frame {i}: {stamp} vs {expected}");
        }
    }

    #[test]
    fn reordered_stream_records_every_frame_on_the_camera_clock() {
        let stream = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bframes_320x180_30fps.h264")).unwrap();
        // one access unit per picture: the parameter sets and SEI ride with the slice after them
        let mut units = Vec::new();
        let mut unit = Vec::new();
        for nal in openh264::nal_units(&stream) {
            let header = nal.iter().position(|&b| b == 1).unwrap() + 1;
            unit.extend_from_slice(nal);
            if matches!(nal[header] & 0x1f, 1 | 5) {
                units.push(std::mem::take(&mut unit));
            }
        }
        assert_eq!(units.len(), 60);
        let mut packetizer = webrtc::rtp::packetizer::new_packetizer(
            1200,
            102,
            0x1234_5678,
            Box::new(webrtc::rtp::codecs::h264::H264Payloader::default()),
            Box::new(webrtc::rtp::sequence::new_random_sequencer()),
            90_000,
        );
        let dir = std::env::temp_dir().join(format!("go2_camera_reorder_{}", now_ns()));
        let path = dir.join("c.mcap");
        let options = crate::record::RecordOptions { format: "mcap".into(), ..Default::default() };
        let recorder = Arc::new(Recorder::start_with_options(&path, Default::default(), |_| Default::default(), options).unwrap());
        let tap = start(recorder.clone(), Arc::new(|| {}));
        // RTP timestamps rise in send order and don't wrap here: webrtc-rs's sample builder subtracts them, which
        // overflows (panics in a debug build only) when one goes back or wraps; CaptureClock's test covers both
        let mut rtp = 1_000_000u32;
        let started = std::time::Instant::now();
        for unit in units {
            rtp = rtp.wrapping_add(3000);
            for mut packet in packetizer.packetize(&bytes::Bytes::from(unit), 90_000 / 30).unwrap() {
                packet.header.timestamp = rtp;
                assert!(tap.push(packet));
            }
            // in real time, as the robot sends them: the clock is placed by the arrival times
            std::thread::sleep(std::time::Duration::from_micros(33_333));
        }
        let sent = started.elapsed();
        // the sample builder holds the last picture until the next timestamp arrives
        for mut packet in packetizer.packetize(&bytes::Bytes::from_static(&[0, 0, 0, 1, 9, 0xF0]), 90_000 / 30).unwrap() {
            packet.header.timestamp = rtp.wrapping_add(3000);
            assert!(tap.push(packet));
        }
        drop(tap);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        recorder.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let images: Vec<_> =
            mcap::MessageStream::new(&bytes).unwrap().map(|m| m.unwrap()).filter(|m| m.channel.topic == "color_image").collect();
        // all 60 but the one or two the decoder still holds for reordering when the stream ends
        assert!(images.len() >= 57, "{} of 60 frames recorded", images.len());
        // source times: in display order, spread over the time the frames took to arrive (CaptureClock measures the
        // RTP clock's rate from the arrivals), not bunched at decode time
        let stamps: Vec<i64> = images.iter().map(|m| m.publish_time as i64).collect();
        assert!(stamps.windows(2).all(|pair| pair[1] > pair[0]), "{stamps:?}");
        let span_ms = (stamps[stamps.len() - 1] - stamps[0]) as f64 / 1e6;
        let sent_ms = sent.as_secs_f64() * 1e3 * (stamps.len() - 1) as f64 / 59.0;
        assert!((span_ms - sent_ms).abs() < sent_ms * 0.25, "stamps span {span_ms} ms, sent over {sent_ms} ms");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    /// 720p keyframes can span several hundred RTP packets. A sample builder that gives up on a frame after 64 late
    /// packets hands the decoder a truncated keyframe; the frames referencing it then fail to decode.
    #[test]
    fn large_keyframes_record_every_frame() {
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, QpRange, RateControlMode};
        use webrtc::rtp::packetizer::Packetizer;
        let (w, h, fps, frames) = (1280usize, 720usize, 15u32, 90usize);
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(fps as f32))
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
        // a fixed noisy texture (compresses badly: big keyframes) with a moving bar (small P-frames)
        let texture: Vec<u8> = (0..w * h * 3).map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let dir = std::env::temp_dir().join(format!("go2_camera_big_{}", now_ns()));
        let path = dir.join("c.mcap");
        let options = crate::record::RecordOptions { format: "mcap".into(), ..Default::default() };
        let recorder = Arc::new(Recorder::start_with_options(&path, Default::default(), |_| Default::default(), options).unwrap());
        let tap = start(recorder.clone(), Arc::new(|| {}));
        let mut biggest = 0;
        for t in 0..frames {
            let mut rgb = texture.clone();
            let bar = (t * 8) % w;
            for y in 0..h {
                for x in bar..(bar + 16).min(w) {
                    let i = (y * w + x) * 3;
                    rgb[i..i + 3].copy_from_slice(&[255, 255, 255]);
                }
            }
            let yuv = openh264::formats::YUVBuffer::from_rgb8_source(openh264::formats::RgbSliceU8::new(&rgb, (w, h)));
            let annex_b = encoder.encode(&yuv).unwrap().to_vec();
            let packets = packetizer.packetize(&bytes::Bytes::from(annex_b), 90_000 / fps).unwrap();
            biggest = biggest.max(packets.len());
            for packet in packets {
                assert!(tap.push(packet));
            }
            std::thread::sleep(std::time::Duration::from_millis(70));
        }
        drop(tap);
        recorder.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let images = mcap::MessageStream::new(&bytes).unwrap().map(|m| m.unwrap()).filter(|m| m.channel.topic == "color_image").count();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(biggest > 64, "the test needs a keyframe over 64 packets, biggest was {biggest}");
        assert!(images >= frames - 10, "{images} of {frames} frames recorded (biggest frame {biggest} packets)");
    }
}
