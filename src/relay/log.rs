//! Relay log: one line per event, appended to a file per box.
//!
//! Nothing secret may be passed to [`Log::line`]. Callers log hosts, paths and
//! status codes, never headers or bodies.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Log {
    file: Option<Arc<Mutex<File>>>,
    /// Also print refusals to the terminal, so the user sees them while `up` runs.
    echo: bool,
    /// Refusals already shown, so a host the agent keeps asking for is printed once.
    shown: Arc<Mutex<HashSet<String>>>,
}

impl Log {
    pub fn to_file(path: &Path, echo: bool) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Log { file: Some(Arc::new(Mutex::new(file))), echo, shown: Arc::default() })
    }

    /// A log that discards everything; for tests.
    #[cfg(test)]
    pub fn discard() -> Self {
        Log { file: None, echo: false, shown: Arc::default() }
    }

    pub fn line(&self, message: &str) {
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = writeln!(file, "{} {message}", timestamp(now()));
            }
        }
    }

    /// Like [`Log::line`], and shown on the terminal the first time it happens.
    pub fn refused(&self, message: &str) {
        self.line(message);
        let first_time = self.shown.lock().map(|mut shown| shown.insert(message.to_string())).unwrap_or(false);
        if self.echo && first_time {
            eprintln!("relay: {message}");
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// UTC timestamp like `2026-11-12T16:30:05Z` from seconds since 1970.
fn timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Civil date from day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_dates() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(timestamp(1_794_501_005), "2026-11-12T16:30:05Z");
    }
}
