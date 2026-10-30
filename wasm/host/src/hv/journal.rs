//! Append-only `journal.jsonl`, the single source of truth, and its stable
//! schema. The hypervisor owns the schema so that every kernel image, old or
//! new, can replay the same journal: images may only write the event types
//! below (or free-form `ext_*` events), and only the hypervisor writes
//! `image_*` events.

use serde_json::{Map, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy)]
enum Ty {
    Str,
    Int,
    Bool,
}

/// Event types a kernel image may write, with their required fields.
const GUEST_EVENTS: &[(&str, &[(&str, Ty)])] = &[
    ("def_proposed", &[("name", Ty::Str), ("kind", Ty::Str), ("source", Ty::Str)]),
    ("def_rejected", &[("name", Ty::Str), ("version", Ty::Int), ("error", Ty::Str)]),
    ("def_trial", &[("name", Ty::Str), ("version", Ty::Int), ("kind", Ty::Str), ("source", Ty::Str)]),
    ("def_use", &[("name", Ty::Str), ("version", Ty::Int), ("ok", Ty::Bool)]),
    ("def_kept", &[("name", Ty::Str), ("version", Ty::Int)]),
    ("def_reverted", &[("name", Ty::Str), ("version", Ty::Int)]),
    ("lesson_added", &[("id", Ty::Int), ("text", Ty::Str)]),
    ("lesson_scored", &[("id", Ty::Int), ("score", Ty::Int)]),
    ("lesson_retired", &[("id", Ty::Int)]),
    ("task_started", &[("task_id", Ty::Int), ("task", Ty::Str)]),
    ("task_finished", &[("task_id", Ty::Int), ("success", Ty::Bool)]),
];

/// Event types only the hypervisor writes (kernel image lifecycle).
const HOST_EVENTS: &[(&str, &[(&str, Ty)])] = &[
    ("image_proposed", &[("version", Ty::Int), ("parent", Ty::Int), ("description", Ty::Str)]),
    ("image_rejected", &[("version", Ty::Int), ("stage", Ty::Str), ("error", Ty::Str), ("verdict", Ty::Str)]),
    ("image_trial", &[("version", Ty::Int), ("parent", Ty::Int), ("description", Ty::Str)]),
    ("image_use", &[("version", Ty::Int), ("ok", Ty::Bool)]),
    ("image_kept", &[("version", Ty::Int), ("verdict", Ty::Str)]),
    ("image_reverted", &[("version", Ty::Int), ("restored", Ty::Int), ("reason", Ty::Str), ("verdict", Ty::Str)]),
];

/// Largest single event an image may append.
const EVENT_MAX: usize = 1 << 20;

pub struct Journal {
    path: PathBuf,
    file: File,
}

fn check_fields(kind: &str, ev: &Map<String, Value>, fields: &[(&str, Ty)]) -> Result<(), String> {
    for (name, ty) in fields {
        let ok = match (ev.get(*name), ty) {
            (Some(Value::String(_)), Ty::Str) => true,
            (Some(v), Ty::Int) => v.is_i64() || v.is_u64(),
            (Some(Value::Bool(_)), Ty::Bool) => true,
            _ => false,
        };
        if !ok {
            let want = match ty {
                Ty::Str => "a string",
                Ty::Int => "an integer",
                Ty::Bool => "a boolean",
            };
            return Err(format!("journal schema: `{kind}` needs field `{name}` as {want}"));
        }
    }
    Ok(())
}

/// Checks an event from a kernel image against the stable schema.
pub fn check_guest_event(ev: &Value) -> Result<(), String> {
    let obj = ev.as_object().ok_or("journal schema: an event must be a JSON object")?;
    let kind = obj.get("type").and_then(Value::as_str).ok_or("journal schema: an event needs a string `type`")?;
    if kind.starts_with("ext_") {
        return Ok(());
    }
    if HOST_EVENTS.iter().any(|(k, _)| *k == kind) {
        return Err(format!("journal schema: `{kind}` events are written only by the hypervisor"));
    }
    let Some((_, fields)) = GUEST_EVENTS.iter().find(|(k, _)| *k == kind) else {
        let known: Vec<&str> = GUEST_EVENTS.iter().map(|(k, _)| *k).collect();
        return Err(format!("journal schema: unknown event type `{kind}` (known: {}, or ext_* for new kinds)", known.join(", ")));
    };
    check_fields(kind, obj, fields)
}

impl Journal {
    pub fn open(state_dir: &Path) -> Result<Journal, String> {
        std::fs::create_dir_all(state_dir).map_err(|e| format!("create {}: {e}", state_dir.display()))?;
        let path = state_dir.join("journal.jsonl");
        let file = OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
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

    /// Appends an event from a kernel image: schema check, then `ts` and
    /// `image` (the version that wrote it) are stamped by the hypervisor.
    pub fn append_guest(&mut self, ev: &Value, image: u32) -> Result<Value, String> {
        check_guest_event(ev)?;
        let mut obj = ev.as_object().cloned().unwrap_or_default();
        obj.insert("image".into(), Value::from(image));
        let kind = obj.remove("type").and_then(|t| t.as_str().map(String::from)).unwrap_or_default();
        self.write(&kind, obj)
    }

    /// Appends a hypervisor event (`image_*`, or `task_finished` for a task
    /// whose image could not finish it).
    pub fn append_host(&mut self, kind: &str, fields: Value) -> Result<Value, String> {
        let obj = match fields {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        if let Some((_, f)) = HOST_EVENTS.iter().chain(GUEST_EVENTS).find(|(k, _)| *k == kind) {
            check_fields(kind, &obj, f)?;
        }
        self.write(kind, obj)
    }

    fn write(&mut self, kind: &str, fields: Map<String, Value>) -> Result<Value, String> {
        let mut obj = Map::new();
        obj.insert("type".into(), Value::String(kind.into()));
        obj.insert("ts".into(), Value::String(now_iso()));
        obj.extend(fields.into_iter().filter(|(k, _)| k != "type" && k != "ts"));
        let ev = Value::Object(obj);
        let mut line = serde_json::to_string(&ev).map_err(|e| e.to_string())?;
        if line.len() > EVENT_MAX {
            return Err(format!("journal: event of {} bytes exceeds the {EVENT_MAX} byte limit", line.len()));
        }
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
    format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, d.subsec_millis())
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
