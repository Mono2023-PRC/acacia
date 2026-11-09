//! Per-run JSONL trace: one record per event, flushed immediately, so a crash still leaves the last
//! tool call visible.
//!
//! `t` is milliseconds since the run started. The API key never appears here: the config is recorded
//! through `Config::redacted()`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub struct Trace {
    path: PathBuf,
    file: BufWriter<File>,
    start: Instant,
}

impl Trace {
    /// Creates `<dir>/<unix>-<slug>.jsonl` and writes the `run_start` record.
    pub fn open(dir: &Path, task: &str, meta: Value) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = dir.join(format!("{stamp}-{}.jsonl", slug(task)));
        let file = File::create(&path)?;
        let mut trace = Trace { path, file: BufWriter::new(file), start: Instant::now() };
        trace.record("run_start", json!({"task": task, "meta": meta}));
        Ok(trace)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one record and flushes it.
    pub fn record(&mut self, record: &str, fields: Value) {
        let mut line = match fields {
            Value::Object(map) => Value::Object(map),
            other => json!({"value": other}),
        };
        line["t"] = json!(self.start.elapsed().as_millis() as u64);
        line["rec"] = json!(record);
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
    }
}

fn slug(task: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for character in task.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            last_dash = false;
            Some(character.to_ascii_lowercase())
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(character) = mapped {
            out.push(character);
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() { "run".to_owned() } else { trimmed }
}
