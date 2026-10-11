// A recording as a dimos memory store (.db): the tables dimos' SqliteStore (dimos/memory/store/sqlite.py) reads, so
// `SqliteStore(path=…)`, `store.replay()` and `dimos --replay` open it as they would one dimos recorded. dimos'
// recorder store (record.rs) writes the rows; this registers the streams and finishes a killed run's file.
//   _streams          name → the stream's config JSON (payload class, codec id, the SQLite blob/vector stores)
//   <name>            id, ts (source time, s), pose_* (left NULL), tags {"reception_ts": …}
//   <name>_blob       id → the encoded message (msg.rs: LCM, JPEG-in-LCM, or LZ4-framed LCM)
//   <name>_rtree      id → a pose, for dimos' spatial queries (empty)
// WAL with a commit at least once a second, so a killed run loses at most that second; the -wal it leaves is folded
// back in on the next start (`recover`), leaving one self-contained file like a finished recording.

use std::path::{Path, PathBuf};

use dimos_memory_recorder::StreamConfig;
use rusqlite::{params, Connection};

/// Creates the file with each stream in `_streams`, byte for byte what SqliteStore._serialize_backend registers.
pub fn register(path: &Path, streams: &[StreamConfig]) -> Result<(), String> {
    let connection = Connection::open(path).map_err(|e| format!("could not create {}: {e}", path.display()))?;
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS _streams (name TEXT PRIMARY KEY, config TEXT NOT NULL)")
        .map_err(|e| e.to_string())?;
    for stream in streams {
        let (payload_type, codec) = (&stream.payload_type, serde_json::to_value(stream.codec).unwrap());
        let codec = codec.as_str().unwrap_or_default();
        let config = format!(
            r#"{{"payload_module": "{payload_type}", "codec_id": "{codec}", "eager_blobs": false, "page_size": 256, "blob_store": {{"class": "dimos.memory.blobstore.sqlite.SqliteBlobStore", "config": {{"path": null}}}}, "vector_store": {{"class": "dimos.memory.vectorstore.sqlite.SqliteVectorStore", "config": {{"path": null}}}}, "notifier": {{"class": "dimos.memory.notifier.subject.SubjectNotifier", "config": {{}}}}}}"#
        );
        connection
            .execute("INSERT OR REPLACE INTO _streams (name, config) VALUES (?1, ?2)", params![stream.name, config])
            .map_err(|e| e.to_string())?;
    }
    connection.close().map_err(|(_, e)| e.to_string())
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
        count += connection.query_row(&format!(r#"SELECT count(*) FROM "{name}""#), [], |row| row.get::<_, i64>(0))? as u64;
    }
    Ok(count)
}
