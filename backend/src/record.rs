// The mcap writer behind the Record button: one file per recording, ROS2 CDR messages with ros2msg schemas (cdr.rs, the
// same encoding Controller's recorder uses), zstd chunks.
//
// Crash-safe and bounded: a writer thread drains a queue capped by bytes (a full queue drops the newest message and
// counts it, it never grows), and closes a chunk at least once a second, so a run killed mid-way leaves every message
// up to that second readable. A file left without its summary (the app was killed) gets one on the next start
// (`recover`). Nothing ever-growing is recorded: the lidar is the robot's local voxel window, not a global map.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mcap::{WriteOptions, Writer};

use crate::cdr::Encoded;

/// messages, whatever their size
const QUEUE_DEPTH: usize = 1024;
/// bytes waiting in the queue: past this new messages are dropped (and counted), so memory stays flat
const QUEUE_BYTES: u64 = 64 * 1024 * 1024;
/// a chunk is closed at least this often: what a hard kill can lose
const FLUSH_EVERY: Duration = Duration::from_secs(1);
const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
pub const PROFILE: &str = "ros2";
pub const LIBRARY: &str = concat!("dim-go2-dash ", env!("CARGO_PKG_VERSION"));

pub const TOPICS: &[&str] = &[
    "/color_image",
    "/camera_info",
    "/lidar",
    "/odom",
    "/tf",
    "/imu",
    "/battery",
    "/joint_states",
    "/joystick",
    "/cmd_vel",
    "/robot_action",
    "/logs",
];

#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordOptions {
    pub directory: String,
    pub compression: String,
    pub image_format: String,
    pub record_new: bool,
    pub logs: bool,
    pub topics: BTreeMap<String, bool>,
    pub rates: BTreeMap<String, f64>,
}
impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            directory: String::new(),
            compression: "zstd".into(),
            image_format: "jpeg-high".into(),
            record_new: true,
            logs: true,
            topics: BTreeMap::new(),
            rates: BTreeMap::new(),
        }
    }
}
impl RecordOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !["zstd", "none"].contains(&self.compression.as_str()) {
            return Err("compression must be zstd or none".into());
        }
        if self.image_format != "raw" && jpeg_quality(&self.image_format).is_none() {
            return Err("imageFormat must be jpeg, jpeg-high, jpeg-best or raw".into());
        }
        if !self.directory.is_empty() && !Path::new(&self.directory).is_absolute() {
            return Err("recording folder must be an absolute path".into());
        }
        if self.rates.values().any(|hz| !hz.is_finite() || *hz <= 0.0 || *hz > 1000.0) {
            return Err("max rates must be between 0 and 1000 Hz (empty = unlimited)".into());
        }
        Ok(())
    }
}

/// The JPEG quality of an image format ("jpeg" small, "jpeg-high", "jpeg-best"); None for raw or unknown.
pub fn jpeg_quality(image_format: &str) -> Option<u8> {
    match image_format {
        "jpeg" => Some(80),
        "jpeg-high" => Some(92),
        "jpeg-best" => Some(98),
        _ => None,
    }
}

struct Sample {
    topic: &'static str,
    encoded: Encoded,
    log_time: u64,
    publish_time: u64,
}

#[derive(Default)]
pub struct Counters {
    pub messages: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
    queued_bytes: AtomicU64,
    per_topic: Mutex<BTreeMap<&'static str, u64>>,
    per_topic_bytes: Mutex<BTreeMap<&'static str, u64>>,
    skipped: AtomicU64,
}

pub struct Recorder {
    options: Mutex<RecordOptions>,
    last_sample: Mutex<HashMap<&'static str, u64>>,
    pub path: PathBuf,
    pub started_ms: u64,
    started: Instant,
    counters: Arc<Counters>,
    sender: Mutex<Option<SyncSender<Sample>>>,
    worker: Mutex<Option<JoinHandle<Result<(), String>>>>,
}

/// Channel metadata a topic carries (the Joy layout, what a stream is), written once with the channel.
pub type ChannelMetadata = fn(&str) -> BTreeMap<String, String>;

impl Recorder {
    /// Creates the file and its writer thread. `metadata` becomes an mcap Metadata record named "dimos.recording".
    pub fn start(path: &Path, metadata: BTreeMap<String, String>, channel_metadata: ChannelMetadata) -> Result<Recorder, String> {
        Self::start_with_options(path, metadata, channel_metadata, RecordOptions::default())
    }

    pub fn start_with_options(
        path: &Path,
        metadata: BTreeMap<String, String>,
        channel_metadata: ChannelMetadata,
        settings: RecordOptions,
    ) -> Result<Recorder, String> {
        settings.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let file = File::create(path).map_err(|e| format!("could not create {}: {e}", path.display()))?;
        let mut writer = options()
            .compression(if settings.compression == "none" { None } else { Some(mcap::Compression::Zstd) })
            .create(BufWriter::new(file))
            .map_err(|e| e.to_string())?;
        writer.write_metadata(&mcap::records::Metadata { name: "dimos.recording".into(), metadata }).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        let (sender, receiver) = sync_channel(QUEUE_DEPTH);
        let counters = Arc::new(Counters::default());
        let worker = {
            let counters = counters.clone();
            std::thread::Builder::new()
                .name("mcap-writer".into())
                .spawn(move || drain(writer, receiver, counters, channel_metadata))
                .map_err(|e| e.to_string())?
        };
        Ok(Recorder {
            options: Mutex::new(settings),
            last_sample: Mutex::new(HashMap::new()),
            path: path.to_path_buf(),
            started_ms: crate::app::now_ms(),
            started: Instant::now(),
            counters,
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Queues a message; never blocks. `publish_time` is the message's own time (its header stamp), `log_time` now.
    pub fn write(&self, topic: &'static str, encoded: Encoded, publish_time: Option<u64>) {
        let now = now_ns();
        let settings = self.options.lock().unwrap();
        let enabled = settings.topics.get(topic).copied().unwrap_or(settings.record_new || TOPICS.contains(&topic));
        let limited = if let Some(hz) = settings.rates.get(topic) {
            let mut last = self.last_sample.lock().unwrap();
            let previous = last.get(topic).copied().unwrap_or(0);
            if now.saturating_sub(previous) < (1e9 / hz) as u64 {
                true
            } else {
                last.insert(topic, now);
                false
            }
        } else {
            false
        };
        if !enabled || limited || (topic == "/logs" && !settings.logs) {
            self.counters.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        drop(settings);
        let Some(sender) = self.sender.lock().unwrap().clone() else { return };
        let size = encoded.data.len() as u64;
        if self.counters.queued_bytes.load(Ordering::Relaxed) + size > QUEUE_BYTES {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let log_time = now_ns();
        self.counters.queued_bytes.fetch_add(size, Ordering::Relaxed);
        let sample = Sample { topic, encoded, log_time, publish_time: publish_time.unwrap_or(log_time) };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = sender.try_send(sample) {
            self.counters.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn configure(&self, settings: RecordOptions) {
        *self.options.lock().unwrap() = settings;
    }
    pub fn image_format(&self) -> String {
        self.options.lock().unwrap().image_format.clone()
    }
    pub fn logs_on(&self) -> bool {
        self.options.lock().unwrap().logs
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "active": true,
            "path": self.path.display().to_string(),
            "file": self.path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "startedAt": self.started_ms,
            "seconds": self.started.elapsed().as_secs_f64(),
            "messages": self.counters.messages.load(Ordering::Relaxed),
            "bytes": self.counters.bytes.load(Ordering::Relaxed),
            "dropped": self.counters.dropped.load(Ordering::Relaxed),
            "topics": *self.counters.per_topic.lock().unwrap(),
            "skipped": self.counters.skipped.load(Ordering::Relaxed),
            "streams": self.counters.per_topic_bytes.lock().unwrap().iter().map(|(topic, bytes)| serde_json::json!({ "topic": topic, "bytesPerSecond": *bytes as f64 / self.started.elapsed().as_secs_f64().max(1.0) })).collect::<Vec<_>>(),
        })
    }

    /// Writes what's queued, the summary and the footer, and closes the file.
    pub fn finish(&self) -> Result<serde_json::Value, String> {
        drop(self.sender.lock().unwrap().take());
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.join().map_err(|_| "the mcap writer thread panicked".to_string())??;
        }
        let mut status = self.status();
        status["active"] = serde_json::json!(false);
        Ok(status)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        drop(self.sender.lock().unwrap().take());
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
    }
}

fn options() -> WriteOptions {
    WriteOptions::new().compression(Some(mcap::Compression::Zstd)).chunk_size(Some(CHUNK_SIZE)).profile(PROFILE).library(LIBRARY)
}

fn drain(
    mut writer: Writer<BufWriter<File>>,
    receiver: Receiver<Sample>,
    counters: Arc<Counters>,
    channel_metadata: ChannelMetadata,
) -> Result<(), String> {
    let mut channels: HashMap<&'static str, (u16, u32)> = HashMap::new();
    let mut last_flush = Instant::now();
    loop {
        let sample = match receiver.recv_timeout(FLUSH_EVERY) {
            Ok(sample) => Some(sample),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(sample) = sample {
            let size = sample.encoded.data.len() as u64;
            counters.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            let slot = match channels.get_mut(sample.topic) {
                Some(slot) => slot,
                None => {
                    let schema = writer
                        .add_schema(sample.encoded.schema_name, "ros2msg", sample.encoded.schema_text.as_bytes())
                        .map_err(|e| e.to_string())?;
                    let id = writer.add_channel(schema, sample.topic, "cdr", &channel_metadata(sample.topic)).map_err(|e| e.to_string())?;
                    channels.entry(sample.topic).or_insert((id, 0))
                }
            };
            slot.1 = slot.1.wrapping_add(1);
            let header = mcap::records::MessageHeader {
                channel_id: slot.0,
                sequence: slot.1,
                log_time: sample.log_time,
                publish_time: sample.publish_time,
            };
            writer.write_to_known_channel(&header, &sample.encoded.data).map_err(|e| e.to_string())?;
            counters.messages.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(size, Ordering::Relaxed);
            *counters.per_topic.lock().unwrap().entry(sample.topic).or_default() += 1;
            *counters.per_topic_bytes.lock().unwrap().entry(sample.topic).or_default() += size;
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            writer.flush().map_err(|e| e.to_string())?;
            last_flush = Instant::now();
        }
    }
    writer.finish().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Whether a file ends with mcap's footer magic (a finished recording) — a killed run's doesn't.
pub fn is_finished(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = File::open(path) else { return false };
    let mut tail = [0u8; 8];
    file.seek(SeekFrom::End(-8)).is_ok() && file.read_exact(&mut tail).is_ok() && tail == *mcap::MAGIC
}

/// Rewrites an unfinished recording (a killed run's: chunks but no summary) with a summary and footer, keeping every
/// message that was written whole. Streams through a memory map, so a big file costs no RAM. Returns the message count.
pub fn recover(path: &Path) -> Result<u64, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    // SAFETY: the file isn't being written (it's an earlier run's); a concurrent truncation would only end the read early
    let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| e.to_string())?;
    let temp = path.with_extension("mcap.recovering");
    let out = File::create(&temp).map_err(|e| e.to_string())?;
    let mut writer = options().create(BufWriter::new(out)).map_err(|e| e.to_string())?;
    let mut count = 0u64;
    let mut channels: HashMap<u16, u16> = HashMap::new();
    let mut schemas: HashMap<u16, u16> = HashMap::new();
    for record in mcap::read::LinearReader::new_with_options(&map, mcap::read::Options::IgnoreEndMagic.into()).map_err(|e| e.to_string())? {
        let Ok(record) = record else { break };
        match record {
            mcap::records::Record::Metadata(metadata) => {
                let _ = writer.write_metadata(&metadata);
            }
            mcap::records::Record::Chunk { header, data } => {
                let Ok(records) = mcap::read::ChunkReader::new(header, &data) else { break };
                for inner in records {
                    let Ok(inner) = inner else { break };
                    match inner {
                        mcap::records::Record::Schema { header, data } => {
                            if let Ok(id) = writer.add_schema(&header.name, &header.encoding, &data) {
                                schemas.insert(header.id, id);
                            }
                        }
                        mcap::records::Record::Channel(channel) => {
                            let schema = schemas.get(&channel.schema_id).copied().unwrap_or(0);
                            if let Ok(id) = writer.add_channel(schema, &channel.topic, &channel.message_encoding, &channel.metadata) {
                                channels.insert(channel.id, id);
                            }
                        }
                        mcap::records::Record::Message { header, data } => {
                            if let Some(&channel_id) = channels.get(&header.channel_id) {
                                let header = mcap::records::MessageHeader { channel_id, ..header };
                                if writer.write_to_known_channel(&header, &data).is_ok() {
                                    count += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    writer.finish().map_err(|e| e.to_string())?;
    drop(writer);
    drop(map);
    std::fs::rename(&temp, path).map_err(|e| e.to_string())?;
    Ok(count)
}

/// A file name for a recording: `2026-10-08_14-32-05_<robot name>.mcap` (local time, then the dog's name, made safe
/// for every filesystem).
pub fn file_name(local_time: &str, robot_name: &str) -> String {
    let name = safe_name(robot_name);
    if name.is_empty() {
        format!("{local_time}.mcap")
    } else {
        format!("{local_time}_{name}.mcap")
    }
}

/// Keeps letters, digits, `-`, `_`, `.` and spaces-as-`_`; drops the rest (`/`, `:`, …), no leading dots.
pub fn safe_name(text: &str) -> String {
    let mapped: String = text
        .trim()
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let mut collapsed = String::new();
    for c in mapped.chars() {
        if !(c == '_' && collapsed.ends_with('_')) {
            collapsed.push(c);
        }
    }
    collapsed.trim_matches(|c| c == '.' || c == '_').chars().take(80).collect()
}

/// Now in local time as `YYYY-MM-DD_HH-MM-SS`.
pub fn local_stamp(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only into `tm`
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return format!("{secs}");
    }
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("go2_record_test_{label}_{}", now_ns()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_metadata(_: &str) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn stream_exclusion_and_rate_caps_are_applied_to_the_actual_mcap() {
        let dir = scratch("options");
        let path = dir.join("filtered.mcap");
        let mut settings = RecordOptions::default();
        settings.compression = "none".into();
        settings.topics.insert("/imu".into(), false);
        settings.rates.insert("/joystick".into(), 1.0);
        let recorder = Recorder::start_with_options(&path, BTreeMap::new(), no_metadata, settings).unwrap();
        recorder.write("/imu", crate::cdr::string("excluded"), None);
        for _ in 0..3 {
            recorder.write("/joystick", crate::cdr::string("sample"), None);
        }
        recorder.write("/robot_action", crate::cdr::string("stand"), None);
        recorder.finish().unwrap();
        let bytes = std::fs::read(path).unwrap();
        let topics: Vec<_> = mcap::MessageStream::new(&bytes).unwrap().map(|m| m.unwrap().channel.topic.clone()).collect();
        assert_eq!(topics.iter().filter(|t| *t == "/joystick").count(), 1);
        assert!(!topics.iter().any(|t| t == "/imu"));
        assert!(topics.iter().any(|t| t == "/robot_action"));
        assert_eq!(recorder.status()["skipped"], 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_cdr_with_schemas_and_reads_back() {
        let dir = scratch("roundtrip");
        let path = dir.join("a.mcap");
        let recorder = Recorder::start(&path, BTreeMap::from([("robot".into(), "Go2".into())]), no_metadata).unwrap();
        for _ in 0..10 {
            recorder.write("/cmd_vel", crate::cdr::twist([0.5, 0.0, 0.0], [0.0, 0.0, 0.1]), None);
        }
        let status = recorder.finish().unwrap();
        assert_eq!(status["messages"], 10);
        assert!(is_finished(&path));
        let bytes = std::fs::read(&path).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes).unwrap().map(|m| m.unwrap()).collect();
        assert_eq!(messages.len(), 10);
        assert_eq!(messages[0].channel.message_encoding, "cdr");
        assert_eq!(messages[0].channel.schema.as_ref().unwrap().name, "geometry_msgs/msg/Twist");
        let summary = mcap::Summary::read(&bytes).unwrap().unwrap();
        assert_eq!(summary.stats.unwrap().message_count, 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_killed_run_is_readable_and_recovers() {
        let dir = scratch("killed");
        let path = dir.join("b.mcap");
        let recorder = Recorder::start(&path, BTreeMap::new(), no_metadata).unwrap();
        for _ in 0..50 {
            recorder.write("/cmd_vel", crate::cdr::twist([0.1, 0.0, 0.0], [0.0; 3]), None);
        }
        // let the writer close a chunk (at most FLUSH_EVERY), then "kill" it: forget the recorder without finishing
        std::thread::sleep(FLUSH_EVERY + Duration::from_millis(300));
        std::mem::forget(recorder);
        assert!(!is_finished(&path));
        let bytes = std::fs::read(&path).unwrap();
        let read = mcap::MessageStream::new_with_options(&bytes, mcap::read::Options::IgnoreEndMagic.into())
            .unwrap()
            .take_while(|m| m.is_ok())
            .count();
        assert_eq!(read, 50, "every flushed message reads back from the unfinished file");
        assert_eq!(recover(&path).unwrap(), 50);
        assert!(is_finished(&path));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(mcap::Summary::read(&bytes).unwrap().unwrap().stats.unwrap().message_count, 50);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names() {
        assert_eq!(file_name("2026-10-08_14-32-05", "Astro / Go2:1"), "2026-10-08_14-32-05_Astro_Go21.mcap");
        assert_eq!(file_name("2026-10-08_14-32-05", "../.."), "2026-10-08_14-32-05.mcap");
        assert_eq!(local_stamp(0).len(), 19);
    }
}
