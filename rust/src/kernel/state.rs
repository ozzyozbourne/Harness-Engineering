//! Registry and knowledge, rebuilt purely by applying journal events.
//!
//! Live operations append an event and then apply it through the same
//! `State::apply`, so replay reproduces exactly the state the process had.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DefVersion {
    pub version: u32,
    pub kind: String,
    pub source: String,
    pub description: String,
    pub tests: Value,
    pub input_schema: Value,
    pub status: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
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
    /// Ends a version's trial with `status`; the soak count starts over.
    fn set_status(&mut self, version: u32, status: &str) {
        if let Some(v) = self.get_mut(version) {
            v.status = status.into();
        }
        self.soak_uses = 0;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lesson {
    pub id: i64,
    pub text: String,
    pub score: i64,
    pub retired: bool,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Verdict {
    pub name: String,
    pub version: u32,
    pub outcome: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    pub id: u64,
    pub task: String,
    pub success: Option<bool>,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub defs: BTreeMap<String, Def>,
    pub lessons: Vec<Lesson>,
    pub verdicts: Vec<Verdict>,
    pub tasks: Vec<TaskRecord>,
}

// Field accessors for journal events and tool inputs. A missing or mistyped
// field reads as empty / zero (`opt_u` as `None`).
pub(crate) fn s(ev: &Value, k: &str) -> String {
    ev.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}
pub(crate) fn opt_u(ev: &Value, k: &str) -> Option<u32> {
    ev.get(k).and_then(Value::as_u64).map(|v| v as u32)
}
pub(crate) fn u(ev: &Value, k: &str) -> u32 {
    opt_u(ev, k).unwrap_or(0)
}
pub(crate) fn u64_field(ev: &Value, k: &str) -> u64 {
    ev.get(k).and_then(Value::as_u64).unwrap_or(0)
}
pub(crate) fn i(ev: &Value, k: &str) -> i64 {
    ev.get(k).and_then(Value::as_i64).unwrap_or(0)
}

impl State {
    pub fn apply(&mut self, ev: &Value) {
        let name = s(ev, "name");
        match ev.get("type").and_then(Value::as_str).unwrap_or("") {
            "def_trial" => {
                let def = self.defs.entry(name.clone()).or_insert_with(|| Def { name, ..Def::default() });
                let version = u(ev, "version");
                def.versions.push(DefVersion {
                    version,
                    kind: s(ev, "kind"),
                    source: s(ev, "source"),
                    description: s(ev, "description"),
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
                    if ok && trial && def.live == Some(u(ev, "version")) {
                        def.soak_uses += 1;
                    }
                }
            }
            "def_kept" => {
                if let Some(def) = self.defs.get_mut(&name) {
                    def.set_status(u(ev, "version"), "kept");
                }
                self.push_verdict(ev, "kept");
            }
            "def_reverted" => {
                if let Some(def) = self.defs.get_mut(&name) {
                    def.set_status(u(ev, "version"), "reverted");
                    def.live = opt_u(ev, "restored");
                }
                self.push_verdict(ev, "reverted");
            }
            "lesson_added" => self.lessons.push(Lesson {
                id: i(ev, "id"),
                text: s(ev, "text"),
                score: 0,
                retired: false,
                origin: s(ev, "origin"),
            }),
            "lesson_scored" => {
                if let Some(l) = self.lesson_mut(i(ev, "id")) {
                    l.score = ev.get("score").and_then(Value::as_i64).unwrap_or(l.score);
                }
            }
            "lesson_retired" => {
                if let Some(l) = self.lesson_mut(i(ev, "id")) {
                    l.retired = true;
                }
            }
            "task_started" => self.tasks.push(TaskRecord {
                id: u64_field(ev, "task_id"),
                task: s(ev, "task"),
                success: None,
                summary: String::new(),
            }),
            "task_finished" => {
                let id = u64_field(ev, "task_id");
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    t.success = ev.get("success").and_then(Value::as_bool);
                    t.summary = s(ev, "summary");
                }
            }
            // def_proposed / def_rejected are history only.
            _ => {}
        }
    }

    fn push_verdict(&mut self, ev: &Value, outcome: &str) {
        if ev.get("seed").and_then(Value::as_bool) == Some(true) {
            return;
        }
        self.verdicts.push(Verdict {
            name: s(ev, "name"),
            version: u(ev, "version"),
            outcome: outcome.into(),
            text: s(ev, "verdict"),
        });
    }

    pub fn live(&self, name: &str) -> Option<&DefVersion> {
        self.defs.get(name).and_then(Def::live_version)
    }

    /// Every definition whose live version is of `kind`, in name order.
    pub fn live_defs(&self, kind: &str) -> impl Iterator<Item = (&Def, &DefVersion)> {
        self.defs.values().filter_map(move |d| d.live_version().filter(|v| v.kind == kind).map(|v| (d, v)))
    }

    pub fn lesson(&self, id: i64) -> Option<&Lesson> {
        self.lessons.iter().find(|l| l.id == id)
    }

    fn lesson_mut(&mut self, id: i64) -> Option<&mut Lesson> {
        self.lessons.iter_mut().find(|l| l.id == id)
    }

    pub fn active_lessons(&self) -> impl Iterator<Item = &Lesson> {
        self.lessons.iter().filter(|l| !l.retired)
    }

    pub fn next_lesson_id(&self) -> i64 {
        self.lessons.iter().map(|l| l.id).max().unwrap_or(0) + 1
    }

    pub fn has_lesson_text(&self, text: &str) -> bool {
        let norm = |t: &str| t.trim().to_lowercase();
        let text = norm(text);
        self.lessons.iter().any(|l| norm(&l.text) == text)
    }
}

pub fn words(s: &str, min: usize) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= min)
        .map(str::to_lowercase)
        .collect()
}
