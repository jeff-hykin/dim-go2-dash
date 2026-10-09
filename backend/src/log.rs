// The debug log: every connect attempt, signaling step, data-channel message (both ways), command and drive call,
// timestamped, appended to <data dir>/go2-ctrl.log (and stderr). Rotated to go2-ctrl.log.1 past LOG_LIMIT.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub const LOG_FILE: &str = "go2-ctrl.log";
const LOG_LIMIT: u64 = 20 * 1024 * 1024;
/// one data-channel message can be a whole lidar frame: keep the start of it
const MAX_LINE: usize = 2000;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init(data_dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let _ = PATH.set(data_dir.join(LOG_FILE));
    line(&format!("=== go2 ctrl {} started (pid {})", env!("CARGO_PKG_VERSION"), std::process::id()));
}

pub fn path() -> Option<PathBuf> {
    PATH.get().cloned()
}

pub fn line(text: &str) {
    let text = if text.len() > MAX_LINE {
        let cut = (0..=MAX_LINE).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        format!("{}… ({} bytes)", &text[..cut], text.len())
    } else {
        text.to_string()
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let stamped = format!("{}.{:03} {text}", crate::robot_rtc::format_utc(now.as_secs()), now.subsec_millis());
    eprintln!("go2_dash: {stamped}");
    let Some(path) = PATH.get() else { return };
    let mut file = FILE.lock().unwrap();
    if file.is_none() || std::fs::metadata(path).map(|m| m.len() > LOG_LIMIT).unwrap_or(true) {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_LIMIT) {
            let _ = std::fs::rename(path, path.with_extension("log.1"));
        }
        *file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
    }
    if let Some(file) = file.as_mut() {
        let _ = writeln!(file, "{stamped}");
    }
}

/// `dlog!("connect {ip}")`: a formatted line in the debug log
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => { $crate::log::line(&format!($($arg)*)) };
}
