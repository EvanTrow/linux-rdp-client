use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOG_FILE: OnceLock<Mutex<Option<fs::File>>> = OnceLock::new();

/// Truncates and opens the AAD automation log for this run. Call once, before any
/// `aad_log!` use — subsequent calls are no-ops (the file's already open for this process).
pub fn init() {
    if LOG_FILE.get().is_some() {
        return;
    }
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .ok();
    let _ = LOG_FILE.set(Mutex::new(file));
    eprintln!("[aad] logging this run's AAD automation output to {}", path.display());
}

pub fn log_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("rdp-client")
        .join("aad-session.log")
}

/// Prints to stderr (same as before) and, if [`init`] was called, also appends to the log
/// file — so a failure can be diagnosed from the file alone without needing the terminal
/// scrollback pasted back in.
pub fn log(args: std::fmt::Arguments) {
    let msg = args.to_string();
    eprintln!("{msg}");
    if let Some(mutex) = LOG_FILE.get() {
        if let Ok(mut guard) = mutex.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{msg}");
                let _ = file.flush();
            }
        }
    }
}

macro_rules! aad_log {
    ($($arg:tt)*) => {
        $crate::aad_auto::log::log(format_args!($($arg)*))
    };
}
pub(crate) use aad_log;
