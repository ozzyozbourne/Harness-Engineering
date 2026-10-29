//! Append-only `journal.jsonl`: the single source of truth.

use serde_json::{Map, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Journal {
    path: PathBuf,
    file: File,
}

impl Journal {
    pub fn open(state_dir: &Path) -> Result<Journal, String> {
        std::fs::create_dir_all(state_dir).map_err(|e| format!("create {}: {e}", state_dir.display()))?;
        let path = state_dir.join("journal.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        Ok(Journal { path, file })
    }

    /// Reads every event. A torn final line (crash mid-write) is skipped.
    pub fn read_all(&self) -> Result<Vec<Value>, String> {
        let f = File::open(&self.path).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line.map_err(|e| e.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => out.push(v),
                Err(e) => eprintln!("[journal] skipping unreadable line {}: {e}", i + 1),
            }
        }
        Ok(out)
    }

    /// Stamps `type` and `ts`, appends one line, and returns the event.
    pub fn append(&mut self, kind: &str, fields: Value) -> Result<Value, String> {
        let mut obj = Map::new();
        obj.insert("type".into(), Value::String(kind.into()));
        obj.insert("ts".into(), Value::String(now_iso()));
        if let Value::Object(m) = fields {
            obj.extend(m);
        }
        let ev = Value::Object(obj);
        let mut line = serde_json::to_string(&ev).map_err(|e| e.to_string())?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        self.file.flush().map_err(|e| e.to_string())?;
        Ok(ev)
    }
}

/// ISO-8601 UTC timestamp with milliseconds.
pub fn now_iso() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, dd) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m as u32, d as u32)
}
