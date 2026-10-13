// What the dog sent, before any parsing: messages and bytes per kind ("msg rt/lf/lowstate", "bin
// rt/utlidar/voxel_map_compressed", "bin ?unparsed" for binary frames parse_binary can't read) and the lidar messages
// that arrive but don't become points ("lidar no-binary", "lidar decode-error"). health.rs writes the per-second deltas
// to a recording's /logs (type rx_stats) and the totals to the log every 10 s, so a stream that stops says which side stopped it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};

static TOTALS: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());
/// the first bytes of the latest binary frame parse_binary couldn't read, as hex
static UNPARSED_HEAD: Mutex<Option<String>> = Mutex::new(None);

pub fn count(kind: &str, bytes: usize) {
    let mut totals = TOTALS.lock().unwrap();
    let entry = totals.entry(kind.to_string()).or_default();
    entry.0 += 1;
    entry.1 += bytes as u64;
}

pub fn unparsed(frame: &[u8]) {
    count("bin ?unparsed", frame.len());
    let head: String = frame.iter().take(24).map(|b| format!("{b:02x}")).collect();
    *UNPARSED_HEAD.lock().unwrap() = Some(head);
}

/// The totals so far: {kind: [messages, bytes]}.
pub fn totals() -> BTreeMap<String, (u64, u64)> {
    TOTALS.lock().unwrap().clone()
}

/// What changed since `before` (an earlier `totals()`), as {kind: {n, bytes}}, plus the latest unreadable frame's head.
pub fn since(before: &BTreeMap<String, (u64, u64)>, now: &BTreeMap<String, (u64, u64)>) -> Value {
    let topics: serde_json::Map<String, Value> = now
        .iter()
        .filter_map(|(kind, (n, bytes))| {
            let (n0, b0) = before.get(kind).copied().unwrap_or((0, 0));
            (*n > n0).then(|| (kind.clone(), json!({ "n": n - n0, "bytes": bytes - b0 })))
        })
        .collect();
    json!({ "topics": topics, "unparsedHead": *UNPARSED_HEAD.lock().unwrap() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_since_an_earlier_snapshot() {
        count("test a", 10);
        let before = totals();
        count("test a", 5);
        count("test a", 5);
        count("test b", 1);
        let delta = since(&before, &totals());
        assert_eq!(delta["topics"]["test a"], json!({ "n": 2, "bytes": 10 }));
        assert_eq!(delta["topics"]["test b"], json!({ "n": 1, "bytes": 1 }));
        unparsed(&[1, 2, 3]);
        assert_eq!(since(&before, &totals())["unparsedHead"], "010203");
    }
}
