//! Registry and knowledge, rebuilt purely by applying journal events.
//!
//! Live operations append an event and then apply it through the same
//! `State::apply`, so replay reproduces exactly the state the process had.
//! The event types are the hypervisor's stable journal schema; unknown
//! types (for example `ext_*` events from a newer image) are ignored.

use crate::util::{i64_field, str_field, u32_field};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct DefVersion {
    pub version: u32,
    pub kind: String,
    pub source: String,
    pub description: String,
    pub tests: Value,
    pub input_schema: Value,
    pub status: String,
}

impl DefVersion {
    pub fn to_json(&self) -> Value {
        json!({"version": self.version, "kind": self.kind, "source": self.source, "description": self.description,
            "tests": self.tests, "input_schema": self.input_schema, "status": self.status})
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Def {
    pub name: String,
    pub versions: Vec<DefVersion>,
    pub live: Option<u32>,
    /// Clean uses counted for the live version while it is `trial`.
    pub soak_uses: u32,
}

impl Def {
    pub fn get(&self, version: u32) -> Option<&DefVersion> {
        self.versions.iter().find(|v| v.version == version)
    }
    fn get_mut(&mut self, version: u32) -> Option<&mut DefVersion> {
        self.versions.iter_mut().find(|v| v.version == version)
    }
    pub fn live_version(&self) -> Option<&DefVersion> {
        self.live.and_then(|v| self.get(v))
    }
    /// The version a failing trial reverts to: the most recent `kept` one.
    pub fn last_kept(&self) -> Option<u32> {
        self.versions.iter().rev().find(|v| v.status == "kept").map(|v| v.version)
    }
    pub fn next_version(&self) -> u32 {
        self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lesson {
    pub id: i64,
    pub text: String,
    pub score: i64,
    pub retired: bool,
    pub origin: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// A definition name, or `image` for kernel image verdicts.
    pub name: String,
    pub version: u32,
    pub outcome: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskRecord {
    pub id: u64,
    pub task: String,
    pub success: Option<bool>,
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub defs: BTreeMap<String, Def>,
    pub lessons: Vec<Lesson>,
    pub verdicts: Vec<Verdict>,
    pub tasks: Vec<TaskRecord>,
}

impl State {
    pub fn apply(&mut self, ev: &Value) {
        let name = str_field(ev, "name");
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "def_trial" => {
                let def = self.defs.entry(name.clone()).or_insert_with(|| Def { name, ..Def::default() });
                let version = u32_field(ev, "version");
                def.versions.push(DefVersion {
                    version,
                    kind: str_field(ev, "kind"),
                    source: str_field(ev, "source"),
                    description: str_field(ev, "description"),
                    tests: ev.get("tests").cloned().unwrap_or(json!([])),
                    input_schema: ev.get("input_schema").cloned().unwrap_or(Value::Null),
                    status: "trial".into(),
                });
                def.live = Some(version);
                def.soak_uses = 0;
            }
            "def_use" => {
                if let Some(def) = self.defs.get_mut(&name) {
                    let ok = ev.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let trial = def.live_version().is_some_and(|v| v.status == "trial");
                    if ok && trial && def.live == Some(u32_field(ev, "version")) {
                        def.soak_uses += 1;
                    }
                }
            }
            "def_kept" => self.settle(&name, ev, "kept"),
            "def_reverted" => self.settle(&name, ev, "reverted"),
            "lesson_added" => self.lessons.push(Lesson {
                id: i64_field(ev, "id"),
                text: str_field(ev, "text"),
                score: 0,
                retired: false,
                origin: str_field(ev, "origin"),
            }),
            "lesson_scored" => {
                let id = i64_field(ev, "id");
                if let Some(l) = self.lessons.iter_mut().find(|l| l.id == id) {
                    // A missing score keeps the old one (not 0).
                    l.score = ev.get("score").and_then(Value::as_i64).unwrap_or(l.score);
                }
            }
            "lesson_retired" => {
                let id = i64_field(ev, "id");
                if let Some(l) = self.lessons.iter_mut().find(|l| l.id == id) {
                    l.retired = true;
                }
            }
            "task_started" => self.tasks.push(TaskRecord {
                id: ev.get("task_id").and_then(Value::as_u64).unwrap_or(0),
                task: str_field(ev, "task"),
                success: None,
                summary: String::new(),
            }),
            "task_finished" => {
                let id = ev.get("task_id").and_then(Value::as_u64).unwrap_or(0);
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    t.success = ev.get("success").and_then(Value::as_bool);
                    t.summary = str_field(ev, "summary");
                }
            }
            // Image verdicts, written by the hypervisor, are knowledge too.
            kind @ ("image_kept" | "image_reverted" | "image_rejected") => {
                let outcome = kind.trim_start_matches("image_");
                self.verdicts.push(Verdict {
                    name: "image".into(),
                    version: u32_field(ev, "version"),
                    outcome: outcome.into(),
                    text: str_field(ev, "verdict"),
                });
            }
            // def_proposed / def_rejected / other image events are history only.
            _ => {}
        }
    }

    /// Ends a soak (`def_kept` / `def_reverted`): the version gets `status`,
    /// a revert makes `restored` live (none if missing), and the verdict is kept.
    fn settle(&mut self, name: &str, ev: &Value, status: &str) {
        if let Some(def) = self.defs.get_mut(name) {
            if let Some(v) = def.get_mut(u32_field(ev, "version")) {
                v.status = status.into();
            }
            if status == "reverted" {
                def.live = ev.get("restored").and_then(Value::as_u64).map(|r| r as u32);
            }
            def.soak_uses = 0;
        }
        if ev.get("seed").and_then(Value::as_bool) == Some(true) {
            return;
        }
        self.verdicts.push(Verdict {
            name: str_field(ev, "name"),
            version: u32_field(ev, "version"),
            outcome: status.into(),
            text: str_field(ev, "verdict"),
        });
    }

    pub fn live(&self, name: &str) -> Option<&DefVersion> {
        self.defs.get(name).and_then(Def::live_version)
    }

    /// Every definition that has a live version, with that version, by name.
    pub fn live_defs(&self) -> impl Iterator<Item = (&Def, &DefVersion)> {
        self.defs.values().filter_map(|d| d.live_version().map(|v| (d, v)))
    }

    pub fn active_lessons(&self) -> impl Iterator<Item = &Lesson> {
        self.lessons.iter().filter(|l| !l.retired)
    }

    pub fn next_lesson_id(&self) -> i64 {
        self.lessons.iter().map(|l| l.id).max().unwrap_or(0) + 1
    }

    pub fn has_lesson_text(&self, text: &str) -> bool {
        let norm = |t: &str| t.trim().to_lowercase();
        self.lessons.iter().any(|l| norm(&l.text) == norm(text))
    }

    /// The whole state as JSON (`/state`): tests and tools compare it.
    pub fn to_json(&self) -> Value {
        let defs: serde_json::Map<String, Value> = self
            .defs
            .values()
            .map(|d| {
                let versions: Vec<Value> = d.versions.iter().map(DefVersion::to_json).collect();
                (d.name.clone(), json!({"live": d.live, "soak_uses": d.soak_uses, "versions": versions}))
            })
            .collect();
        let lessons: Vec<Value> = self
            .lessons
            .iter()
            .map(|l| json!({"id": l.id, "text": l.text, "score": l.score, "retired": l.retired, "origin": l.origin}))
            .collect();
        let verdicts: Vec<Value> =
            self.verdicts.iter().map(|v| json!({"name": v.name, "version": v.version, "outcome": v.outcome, "text": v.text})).collect();
        let tasks: Vec<Value> =
            self.tasks.iter().map(|t| json!({"id": t.id, "task": t.task, "success": t.success, "summary": t.summary})).collect();
        json!({"defs": defs, "lessons": lessons, "verdicts": verdicts, "tasks": tasks})
    }
}
