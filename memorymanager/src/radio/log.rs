//! A plain-text transcript of bridge traffic, for troubleshooting transfers.
//!
//! Each line is `YYYY-MM-DD HH:MM:SS.mmm <marker> <text>`, where the marker is
//! `>` for lines sent to the bridge, `<` for lines received, and `*` for notes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Longer lines are shortened. Diagnostic lines run to about 300 characters;
/// only the 512-digit hex blocks are longer.
const MAX_LINE: usize = 400;

/// Shared, cheap-to-clone handle to the transcript. A disabled logger drops everything.
#[derive(Clone, Default)]
pub struct Logger {
    file: Option<Arc<Mutex<File>>>,
    path: Option<PathBuf>,
}

impl Logger {
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Appends to `path`, creating its folder if needed.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file: Some(Arc::new(Mutex::new(file))), path: Some(path.to_path_buf()) })
    }

    /// The default log file, or a disabled logger if it cannot be opened.
    pub fn open_default() -> Self {
        default_path().and_then(|path| Self::open(&path).ok()).unwrap_or_default()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn sent(&self, line: &str) {
        self.write('>', line);
    }

    pub fn received(&self, line: &str) {
        self.write('<', line);
    }

    pub fn note(&self, text: &str) {
        self.write('*', text);
    }

    fn write(&self, marker: char, text: &str) {
        let Some(file) = &self.file else { return };
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{} {marker} {}", timestamp(), abbreviate(text));
            let _ = file.flush();
        }
    }
}

/// `~/Library/Logs/MemoryManager/radio.log` on macOS,
/// `~/.local/state/memory-manager/radio.log` elsewhere.
pub fn default_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Logs/MemoryManager/radio.log")
    } else {
        home.join(".local/state/memory-manager/radio.log")
    })
}

/// Replaces control characters (boot noise) and shortens long lines such as
/// the 512-digit hex blocks.
pub fn abbreviate(text: &str) -> String {
    let clean: String = text.chars().map(|c| if c.is_control() { '.' } else { c }).collect();
    let count = clean.chars().count();
    if count <= MAX_LINE {
        return clean;
    }
    let head: String = clean.chars().take(70).collect();
    let tail: String = clean.chars().skip(count - 12).collect();
    format!("{head} …({count} chars)… {tail}")
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    // SAFETY: `tm` is plain data, and localtime_r only writes to it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return format!("{}.{:03}", now.as_secs(), now.subsec_millis());
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_marked_timestamped_lines() {
        let dir = std::env::temp_dir().join(format!("mm-log-{}", std::process::id()));
        let path = dir.join("nested/radio.log");
        let logger = Logger::open(&path).unwrap();
        assert_eq!(logger.path(), Some(path.as_path()));
        logger.sent("HELLO");
        logger.clone().received("OK HELLO bridge proto=1");
        logger.note("Read from radio");
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with(" > HELLO"), "{}", lines[0]);
        assert!(lines[1].ends_with(" < OK HELLO bridge proto=1"));
        assert!(lines[2].ends_with(" * Read from radio"));
        assert_eq!(&lines[0][4..5], "-");
        fs::remove_dir_all(&dir).unwrap();

        Logger::disabled().note("nowhere");
    }

    #[test]
    fn long_and_binary_lines_are_shortened() {
        let put = format!("PUT 3 {} 1234ABCD", "0".repeat(512));
        let short = abbreviate(&put);
        assert!(short.len() < 120, "{short}");
        assert!(short.starts_with("PUT 3 000"));
        assert!(short.ends_with("0 1234ABCD"));
        assert!(short.contains("(527 chars)"));
        assert_eq!(abbreviate("a\u{0}b\u{80}c"), "a.b.c");
        assert_eq!(abbreviate("OK DIAG"), "OK DIAG");
    }
}
