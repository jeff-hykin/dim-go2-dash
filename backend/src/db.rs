// A recording as a dimos memory store (.db): the tables dimos' SqliteStore (dimos/memory/store/sqlite.py) writes and
// reads, so `SqliteStore(path=…)`, `store.replay()` and `dimos --replay` open it as they would one dimos recorded.
//   _streams          name → the stream's config JSON (payload class, codec id, the SQLite blob/vector stores)
//   <name>            id, ts (source time, s), pose_* (the robot's latest odom pose), tags {"reception_ts": …}
//   <name>_blob       id → the encoded message (msg.rs: LCM, JPEG-in-LCM, or LZ4-framed LCM)
//   <name>_rtree      id → the pose, for dimos' spatial queries
// WAL with a commit at least once a second (record.rs), so a killed run loses at most that second; the -wal it leaves
// is folded back in on the next start (`recover`), leaving one self-contained file like a finished recording.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::msg::Rows;

pub struct Db {
    connection: Connection,
    streams: HashSet<String>,
    open_transaction: bool,
    pose: Option<[f64; 7]>,
}

impl Db {
    pub fn create(path: &Path) -> Result<Db, String> {
        let connection = Connection::open(path).map_err(|e| format!("could not create {}: {e}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL").map_err(|e| e.to_string())?;
        connection.pragma_update(None, "synchronous", "NORMAL").map_err(|e| e.to_string())?;
        connection.set_prepared_statement_cache_capacity(64);
        connection
            .execute_batch("CREATE TABLE IF NOT EXISTS _streams (name TEXT PRIMARY KEY, config TEXT NOT NULL)")
            .map_err(|e| e.to_string())?;
        Ok(Db { connection, streams: HashSet::new(), open_transaction: false, pose: None })
    }

    /// Writes one message's rows into stream `name` (created on its first message). `pose`: odom's, which every row
    /// from then on is anchored to. Returns the bytes stored.
    pub fn write(&mut self, name: &str, rows: &Rows, pose: Option<[f64; 7]>, reception_ts: f64) -> Result<u64, String> {
        if !self.open_transaction {
            self.connection.execute_batch("BEGIN").map_err(|e| e.to_string())?;
            self.open_transaction = true;
        }
        if !self.streams.contains(name) {
            create_stream(&self.connection, name, rows.payload_type, rows.codec).map_err(|e| e.to_string())?;
            self.streams.insert(name.to_string());
        }
        if pose.is_some() {
            self.pose = pose;
        }
        let is_tf = name == "tf";
        let mut bytes = 0;
        for (ts, data) in &rows.rows {
            let data = if rows.codec.starts_with("lz4+") { lz4_frame(data)? } else { data.clone() };
            insert(&self.connection, name, ts.unwrap_or(reception_ts), reception_ts, if is_tf { None } else { self.pose }, &data)
                .map_err(|e| e.to_string())?;
            bytes += data.len() as u64;
        }
        Ok(bytes)
    }

    /// Commits what's written: what a hard kill can no longer lose.
    pub fn commit(&mut self) -> Result<(), String> {
        if self.open_transaction {
            self.connection.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            self.open_transaction = false;
        }
        Ok(())
    }

    /// Commits, folds the WAL into the file and closes it: one self-contained .db.
    pub fn finish(mut self) -> Result<(), String> {
        self.commit()?;
        settle(self.connection)
    }
}

fn create_stream(connection: &Connection, name: &str, payload_type: &str, codec: &str) -> rusqlite::Result<()> {
    connection.execute_batch(&format!(
        r#"CREATE TABLE IF NOT EXISTS "{name}" (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            ts      REAL    NOT NULL,
            value   NUMERIC,
            pose_x  REAL, pose_y REAL, pose_z REAL,
            pose_qx REAL, pose_qy REAL, pose_qz REAL, pose_qw REAL,
            tags    BLOB    DEFAULT (jsonb('{{}}')));
        CREATE TABLE IF NOT EXISTS "{name}_blob" (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
        CREATE VIRTUAL TABLE IF NOT EXISTS "{name}_rtree" USING rtree(id, x_min, x_max, y_min, y_max, z_min, z_max);"#
    ))?;
    if name != "tf" {
        connection.execute_batch(&format!(
            r#"CREATE INDEX IF NOT EXISTS "{name}_tag_reception_ts" ON "{name}"(json_extract(tags, '$.reception_ts'))"#
        ))?;
    }
    // byte for byte what SqliteStore._serialize_backend registers for a stream it creates
    let config = format!(
        r#"{{"payload_module": "{payload_type}", "codec_id": "{codec}", "eager_blobs": false, "page_size": 256, "blob_store": {{"class": "dimos.memory.blobstore.sqlite.SqliteBlobStore", "config": {{"path": null}}}}, "vector_store": {{"class": "dimos.memory.vectorstore.sqlite.SqliteVectorStore", "config": {{"path": null}}}}, "notifier": {{"class": "dimos.memory.notifier.subject.SubjectNotifier", "config": {{}}}}}}"#
    );
    connection.execute("INSERT OR REPLACE INTO _streams (name, config) VALUES (?1, ?2)", params![name, config])?;
    Ok(())
}

fn insert(connection: &Connection, name: &str, ts: f64, reception_ts: f64, pose: Option<[f64; 7]>, data: &[u8]) -> rusqlite::Result<()> {
    // dimos' Recorder tags each row with its arrival time; its tf rows carry none
    let tags = if name == "tf" { "{}".to_string() } else { format!(r#"{{"reception_ts": {reception_ts}}}"#) };
    let p = pose.map(|p| p.map(Some)).unwrap_or([None; 7]);
    connection
        .prepare_cached(&format!(
            r#"INSERT INTO "{name}" (ts, pose_x, pose_y, pose_z, pose_qx, pose_qy, pose_qz, pose_qw, tags) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, jsonb(?9))"#
        ))?
        .execute(params![ts, p[0], p[1], p[2], p[3], p[4], p[5], p[6], tags])?;
    let id = connection.last_insert_rowid();
    connection.prepare_cached(&format!(r#"INSERT INTO "{name}_blob" (id, data) VALUES (?1, ?2)"#))?.execute(params![id, data])?;
    if let Some(p) = pose {
        connection
            .prepare_cached(&format!(
                r#"INSERT INTO "{name}_rtree" (id, x_min, x_max, y_min, y_max, z_min, z_max) VALUES (?1, ?2, ?2, ?3, ?3, ?4, ?4)"#
            ))?
            .execute(params![id, p[0], p[1], p[2]])?;
    }
    Ok(())
}

fn lz4_frame(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
    encoder.write_all(data).map_err(|e| e.to_string())?;
    encoder.finish().map_err(|e| e.to_string())
}

fn settle(connection: Connection) -> Result<(), String> {
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;").map_err(|e| e.to_string())?;
    connection.close().map_err(|(_, e)| e.to_string())
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// SQLite's companions of a .db (its write-ahead log and shared memory), to move or delete along with it.
pub fn companions(path: &Path) -> [PathBuf; 2] {
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    [wal(path), PathBuf::from(shm)]
}

/// A finished .db is one file; a killed run's leaves its write-ahead log beside it.
pub fn is_finished(path: &Path) -> bool {
    !wal(path).exists()
}

/// Folds a killed run's write-ahead log into its .db (every committed row is kept). Returns the row count.
pub fn recover(path: &Path) -> Result<u64, String> {
    let connection = Connection::open(path).map_err(|e| e.to_string())?;
    let count = row_count(&connection).map_err(|e| e.to_string())?;
    settle(connection)?;
    Ok(count)
}

/// Rows in every registered stream.
pub fn row_count(connection: &Connection) -> rusqlite::Result<u64> {
    let names: Vec<String> = connection.prepare("SELECT name FROM _streams")?.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
    let mut count = 0u64;
    for name in names {
        count += connection.query_row(&format!(r#"SELECT count(*) FROM "{name}""#), [], |row| row.get::<_, u64>(0))?;
    }
    Ok(count)
}
