// The writer behind the Record button: one file per recording, either a dimos memory store (.db, the default: dimos'
// LCM types in SQLite, db.rs) or an mcap (ROS2 CDR messages with ros2msg schemas, cdr.rs, the same encoding
// Controller's recorder uses, zstd chunks). A message is encoded for the file's format as it's queued (msg.rs).
//
// Crash-safe and bounded: a writer thread drains a queue capped by bytes (a full queue drops the newest message and
// counts it, it never grows), and closes a chunk (mcap) or commits (.db) at least once a second, so a run killed
// mid-way leaves every message up to that second readable. A file a killed run left unfinished (an mcap without its
// summary, a .db with its -wal beside it) is finished on the next start (`recover`). Nothing ever-growing is recorded:
// the lidar is the robot's local voxel window, not a global map.

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
use crate::db::Db;
use crate::msg::{Msg, Rows};

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
    /// "db" (a dimos memory store) or "mcap"
    pub format: String,
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
            format: "db".into(),
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
        if !["db", "mcap"].contains(&self.format.as_str()) {
            return Err("format must be db or mcap".into());
        }
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

enum Payload {
    Cdr(Encoded),
    /// .db rows, and the robot pose they carry (odom's)
    Dimos(Rows, Option<[f64; 7]>),
}

impl Payload {
    fn len(&self) -> u64 {
        match self {
            Payload::Cdr(encoded) => encoded.data.len() as u64,
            Payload::Dimos(rows, _) => rows.rows.iter().map(|(_, data)| data.len() as u64).sum(),
        }
    }
}

struct Sample {
    topic: &'static str,
    payload: Payload,
    log_time: u64,
}

enum Sink {
    Mcap { writer: Writer<BufWriter<File>>, channels: HashMap<&'static str, (u16, u32)>, channel_metadata: ChannelMetadata },
    Db(Db),
}

impl Sink {
    /// Writes a sample; returns the bytes it took in the file.
    fn write(&mut self, sample: Sample) -> Result<u64, String> {
        match (self, sample.payload) {
            (Sink::Mcap { writer, channels, channel_metadata }, Payload::Cdr(encoded)) => {
                let slot = match channels.get_mut(sample.topic) {
                    Some(slot) => slot,
                    None => {
                        let schema =
                            writer.add_schema(encoded.schema_name, "ros2msg", encoded.schema_text.as_bytes()).map_err(|e| e.to_string())?;
                        let id = writer.add_channel(schema, sample.topic, "cdr", &channel_metadata(sample.topic)).map_err(|e| e.to_string())?;
                        channels.entry(sample.topic).or_insert((id, 0))
                    }
                };
                slot.1 = slot.1.wrapping_add(1);
                let header =
                    mcap::records::MessageHeader { channel_id: slot.0, sequence: slot.1, log_time: sample.log_time, publish_time: sample.log_time };
                writer.write_to_known_channel(&header, &encoded.data).map_err(|e| e.to_string())?;
                Ok(encoded.data.len() as u64)
            }
            (Sink::Db(db), Payload::Dimos(rows, pose)) => {
                db.write(sample.topic.trim_start_matches('/'), &rows, pose, sample.log_time as f64 / 1e9)
            }
            _ => Err("a message encoded for the other format".into()),
        }
    }

    /// Closes a chunk / commits: what a hard kill can no longer lose.
    fn flush(&mut self) -> Result<(), String> {
        match self {
            Sink::Mcap { writer, .. } => writer.flush().map_err(|e| e.to_string()),
            Sink::Db(db) => db.commit(),
        }
    }

    fn finish(self) -> Result<(), String> {
        match self {
            Sink::Mcap { mut writer, .. } => writer.finish().map(|_| ()).map_err(|e| e.to_string()),
            Sink::Db(db) => db.finish(),
        }
    }
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
    db: bool,
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
    /// Creates the file and its writer thread. `metadata` becomes an mcap Metadata record named "dimos.recording", or a
    /// .db's `metadata` stream: one JSON row, with each topic's channel metadata under "streams".
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
        let db = settings.format == "db";
        let sink = if db {
            let mut db = Db::create(path)?;
            let mut info = serde_json::to_value(&metadata).unwrap();
            info["streams"] = TOPICS.iter().map(|t| (t.trim_start_matches('/').to_string(), serde_json::json!(channel_metadata(t)))).collect();
            db.write("metadata", &Msg::Text(info.to_string()).dimos(false), None, now_ns() as f64 / 1e9)?;
            db.commit()?;
            Sink::Db(db)
        } else {
            let file = File::create(path).map_err(|e| format!("could not create {}: {e}", path.display()))?;
            let mut writer = options()
                .compression(if settings.compression == "none" { None } else { Some(mcap::Compression::Zstd) })
                .create(BufWriter::new(file))
                .map_err(|e| e.to_string())?;
            writer.write_metadata(&mcap::records::Metadata { name: "dimos.recording".into(), metadata }).map_err(|e| e.to_string())?;
            writer.flush().map_err(|e| e.to_string())?;
            Sink::Mcap { writer, channels: HashMap::new(), channel_metadata }
        };
        let (sender, receiver) = sync_channel(QUEUE_DEPTH);
        let counters = Arc::new(Counters::default());
        let worker = {
            let counters = counters.clone();
            std::thread::Builder::new()
                .name("recording-writer".into())
                .spawn(move || drain(sink, receiver, counters))
                .map_err(|e| e.to_string())?
        };
        Ok(Recorder {
            options: Mutex::new(settings),
            db,
            last_sample: Mutex::new(HashMap::new()),
            path: path.to_path_buf(),
            started_ms: crate::app::now_ms(),
            started: Instant::now(),
            counters,
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Queues a message, encoded for the file's format; never blocks.
    pub fn write(&self, topic: &'static str, msg: Msg) {
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
        let compress = settings.compression != "none";
        drop(settings);
        let Some(sender) = self.sender.lock().unwrap().clone() else { return };
        let payload = if self.db {
            let pose = msg.robot_pose();
            Payload::Dimos(msg.dimos(compress), pose)
        } else {
            Payload::Cdr(msg.cdr())
        };
        let size = payload.len();
        if self.counters.queued_bytes.load(Ordering::Relaxed) + size > QUEUE_BYTES {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let log_time = now_ns();
        self.counters.queued_bytes.fetch_add(size, Ordering::Relaxed);
        let sample = Sample { topic, payload, log_time };
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
            worker.join().map_err(|_| "the recording writer thread panicked".to_string())??;
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

fn drain(mut sink: Sink, receiver: Receiver<Sample>, counters: Arc<Counters>) -> Result<(), String> {
    let mut last_flush = Instant::now();
    loop {
        let sample = match receiver.recv_timeout(FLUSH_EVERY) {
            Ok(sample) => Some(sample),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(sample) = sample {
            counters.queued_bytes.fetch_sub(sample.payload.len(), Ordering::Relaxed);
            let topic = sample.topic;
            let size = sink.write(sample)?;
            counters.messages.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(size, Ordering::Relaxed);
            *counters.per_topic.lock().unwrap().entry(topic).or_default() += 1;
            *counters.per_topic_bytes.lock().unwrap().entry(topic).or_default() += size;
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            sink.flush()?;
            last_flush = Instant::now();
        }
    }
    sink.finish()
}

pub fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Whether a recording is finished: an mcap ends with its footer magic, a .db has no -wal beside it (a killed run's do).
pub fn is_finished(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    if is_db(path) {
        return crate::db::is_finished(path);
    }
    let Ok(mut file) = File::open(path) else { return false };
    let mut tail = [0u8; 8];
    file.seek(SeekFrom::End(-8)).is_ok() && file.read_exact(&mut tail).is_ok() && tail == *mcap::MAGIC
}

pub fn is_db(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "db")
}

/// A file this app records: an .mcap or a .db.
pub fn is_recording_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "mcap" || e == "db")
}

/// Finishes an unfinished recording, keeping every message that was written whole: a .db's -wal folded in (db.rs), an
/// mcap (chunks but no summary) rewritten with a summary and footer, streamed through a memory map so a big file costs
/// no RAM. Returns the message count.
pub fn recover(path: &Path) -> Result<u64, String> {
    if is_db(path) {
        return crate::db::recover(path);
    }
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

/// A file name for a recording: `2026-10-08_14-32-05_<robot name>_<machine id>.<format>` (local time, the dog's name,
/// then which deck recorded it; each made safe for every filesystem, empty parts left out).
pub fn file_name(local_time: &str, robot_name: &str, machine_id: &str, format: &str) -> String {
    let mut name = local_time.to_string();
    for part in [safe_name(robot_name), safe_name(machine_id)] {
        if !part.is_empty() {
            name = format!("{name}_{part}");
        }
    }
    format!("{name}.{format}")
}

/// This machine's `machine_id` from Desktop's config.yaml ($DIMOS_HOME, else ~/.dimos; the deck installer writes a
/// random one); empty if there's none.
pub fn machine_id() -> String {
    let home = std::env::var_os("DIMOS_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".dimos")));
    let text = home.and_then(|h| std::fs::read_to_string(h.join("config.yaml")).ok()).unwrap_or_default();
    machine_id_in(&text)
}

/// The top-level `machine_id:` value in a config.yaml (quotes and a trailing comment dropped).
fn machine_id_in(text: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("machine_id:"))
        .map(|value| value.split(" #").next().unwrap_or("").trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .unwrap_or_default()
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

    fn mcap_options() -> RecordOptions {
        RecordOptions { format: "mcap".into(), ..Default::default() }
    }

    #[test]
    fn stream_exclusion_and_rate_caps_are_applied_to_the_actual_mcap() {
        let dir = scratch("options");
        let path = dir.join("filtered.mcap");
        let mut settings = mcap_options();
        settings.compression = "none".into();
        settings.topics.insert("/imu".into(), false);
        settings.rates.insert("/joystick".into(), 1.0);
        let recorder = Recorder::start_with_options(&path, BTreeMap::new(), no_metadata, settings).unwrap();
        recorder.write("/imu", Msg::Text("excluded".into()));
        for _ in 0..3 {
            recorder.write("/joystick", Msg::Text("sample".into()));
        }
        recorder.write("/robot_action", Msg::Text("stand".into()));
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
        let recorder =
            Recorder::start_with_options(&path, BTreeMap::from([("robot".into(), "Go2".into())]), no_metadata, mcap_options()).unwrap();
        for _ in 0..10 {
            recorder.write("/cmd_vel", Msg::Twist([0.5, 0.0, 0.0], [0.0, 0.0, 0.1]));
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
        let recorder = Recorder::start_with_options(&path, BTreeMap::new(), no_metadata, mcap_options()).unwrap();
        for _ in 0..50 {
            recorder.write("/cmd_vel", Msg::Twist([0.1, 0.0, 0.0], [0.0; 3]));
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

    fn db_rows(path: &Path, sql: &str) -> Vec<(String, f64, Option<f64>, Vec<u8>)> {
        let connection = rusqlite::Connection::open(path).unwrap();
        let mut statement = connection.prepare(sql).unwrap();
        statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn writes_a_dimos_store_and_reads_it_back() {
        use lcm_msgs::{geometry_msgs::PoseStamped, sensor_msgs::{Image, PointCloud2}, tf2_msgs::TFMessage};
        let dir = scratch("db");
        let path = dir.join("a.db");
        let recorder = Recorder::start(&path, BTreeMap::from([("robot_name".into(), "Astro".into())]), no_metadata).unwrap();
        let header = |frame_id| crate::cdr::Header { stamp_ns: 1_700_000_000_500_000_000, frame_id };
        recorder.write("/odom", Msg::Pose(header("world"), [1.0, 2.0, 0.3], [0.0, 0.0, 0.0, 1.0]));
        let mount = crate::sensors::camera_mount().to_vec();
        recorder.write("/tf", Msg::Tf(1_700_000_000_500_000_000, mount));
        recorder.write("/color_image", Msg::Jpeg { header: header("camera_optical"), width: 4, height: 2, data: vec![0xFF, 0xD8, 0xFF] });
        recorder.write("/lidar", Msg::Points(header("world"), vec![[1.0, 2.0, 3.0]; 100]));
        recorder.write("/cmd_vel", Msg::Twist([0.5, 0.0, 0.0], [0.0; 3]));
        let status = recorder.finish().unwrap();
        assert_eq!(status["messages"], 5);
        assert!(is_finished(&path), "no -wal left beside a finished .db");

        let streams = db_rows(&path, "SELECT name, 0.0, NULL, CAST(config AS BLOB) FROM _streams ORDER BY name");
        let config = |name: &str| -> serde_json::Value {
            serde_json::from_slice(&streams.iter().find(|s| s.0 == name).unwrap().3).unwrap()
        };
        let names: Vec<_> = streams.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(names, ["cmd_vel", "color_image", "lidar", "metadata", "odom", "tf"]);
        assert_eq!(config("color_image")["codec_id"], "jpeg");
        assert_eq!(config("lidar")["codec_id"], "lz4+lcm");
        assert_eq!(config("odom")["payload_module"], "dimos.msgs.geometry_msgs.PoseStamped.PoseStamped");

        let odom = db_rows(&path, "SELECT 'odom', ts, pose_x, data FROM odom JOIN odom_blob USING (id)");
        assert_eq!(odom[0].1, 1_700_000_000.5);
        assert_eq!(odom[0].2, Some(1.0), "rows carry the robot's pose");
        assert_eq!(PoseStamped::decode(&odom[0].3).unwrap().pose.position.y, 2.0);

        let tf = db_rows(&path, "SELECT 'tf', ts, pose_x, data FROM tf JOIN tf_blob USING (id)");
        assert_eq!(tf.len(), 2, "one row per transform");
        assert_eq!(tf[1].2, None);
        assert_eq!(TFMessage::decode(&tf[1].3).unwrap().transforms[0].child_frame_id, "camera_optical");

        let image = db_rows(&path, "SELECT 'image', ts, pose_x, data FROM color_image JOIN color_image_blob USING (id)");
        let image = Image::decode(&image[0].3).unwrap();
        assert_eq!((image.encoding.as_str(), image.width, image.data.len()), ("jpeg", 4, 3));

        let lidar = db_rows(&path, "SELECT 'lidar', ts, pose_x, data FROM lidar JOIN lidar_blob USING (id)");
        let mut lcm = Vec::new();
        std::io::Read::read_to_end(&mut lz4_flex::frame::FrameDecoder::new(&lidar[0].3[..]), &mut lcm).unwrap();
        assert_eq!(PointCloud2::decode(&lcm).unwrap().width, 100);

        let cmd = db_rows(&path, "SELECT 'cmd', ts, json_extract(tags, '$.reception_ts'), data FROM cmd_vel JOIN cmd_vel_blob USING (id)");
        assert_eq!(Some(cmd[0].1), cmd[0].2, "no stamp: source time is when it arrived");
        let metadata = db_rows(&path, "SELECT 'm', ts, NULL, data FROM metadata JOIN metadata_blob USING (id)");
        let text = lcm_msgs::std_msgs::String::decode(&metadata[0].3).unwrap().data;
        let metadata: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(metadata["robot_name"], "Astro");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_killed_db_run_is_readable_and_recovers() {
        let dir = scratch("killed_db");
        let path = dir.join("b.db");
        let recorder = Recorder::start(&path, BTreeMap::new(), no_metadata).unwrap();
        for _ in 0..50 {
            recorder.write("/cmd_vel", Msg::Twist([0.1, 0.0, 0.0], [0.0; 3]));
        }
        std::thread::sleep(FLUSH_EVERY + Duration::from_millis(300));
        // "kill" it: what's on disk now, copied away from the still-open writer (a dead process holds no lock)
        let live = path.clone();
        let path = dir.join("killed.db");
        std::fs::copy(&live, &path).unwrap();
        std::fs::copy(&crate::db::companions(&live)[0], &crate::db::companions(&path)[0]).unwrap();
        std::mem::forget(recorder);
        assert!(!is_finished(&path));
        assert_eq!(db_rows(&path, "SELECT '', ts, NULL, data FROM cmd_vel JOIN cmd_vel_blob USING (id)").len(), 50);
        assert_eq!(recover(&path).unwrap(), 51, "50 commands and the metadata row");
        assert!(is_finished(&path));
        assert_eq!(db_rows(&path, "SELECT '', ts, NULL, data FROM cmd_vel JOIN cmd_vel_blob USING (id)").len(), 50);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names() {
        assert_eq!(file_name("2026-10-08_14-32-05", "Astro / Go2:1", "", "mcap"), "2026-10-08_14-32-05_Astro_Go21.mcap");
        assert_eq!(file_name("2026-10-08_14-32-05", "../..", "", "db"), "2026-10-08_14-32-05.db");
        assert_eq!(file_name("2026-10-08_14-32-05", "Astro", "3f9a0c71b2d4", "db"), "2026-10-08_14-32-05_Astro_3f9a0c71b2d4.db");
        assert_eq!(file_name("2026-10-08_14-32-05", "", "3f9a0c71b2d4", "mcap"), "2026-10-08_14-32-05_3f9a0c71b2d4.mcap");
        assert_eq!(machine_id_in("desktop:\n  port: 5555\nmachine_id: \"3f9a0c71b2d4\" # deck\n"), "3f9a0c71b2d4");
        assert_eq!(machine_id_in("desktop:\n  machine_id: nested\n"), "");
        assert_eq!(local_stamp(0).len(), 19);
    }
}
