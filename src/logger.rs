//! Logging that works with or without a console. Every line goes to
//! %LOCALAPPDATA%\wtm\wtm.log; when a console exists (debug builds, or
//! release run from a terminal before the subsystem switch) it also prints
//! there — with no console, Rust's stderr writes are silent no-ops.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use windows::Win32::System::SystemInformation::GetLocalTime;

#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => { $crate::logger::log_line(&format!($($arg)*)) };
}

fn log_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("wtm").join("wtm.log"))
}

/// Rotate an oversized log at startup (one .old generation kept).
pub fn init() {
    let Some(path) = log_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 512 * 1024 {
            let _ = std::fs::rename(&path, path.with_extension("log.old"));
        }
    }
    log_line(&format!("wtm {} starting", env!("CARGO_PKG_VERSION")));
}

pub fn log_line(msg: &str) {
    eprintln!("{msg}");
    let Some(path) = log_path() else { return };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let t = unsafe { GetLocalTime() };
        let _ = writeln!(
            f,
            "{:02}-{:02} {:02}:{:02}:{:02}  {msg}",
            t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        );
    }
}
