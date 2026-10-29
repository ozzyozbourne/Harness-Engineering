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

fn s(ev: &Value, k: &str) -> String {
    ev.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}
fn u(ev: &Value, k: &str) -> u32 {
    ev.get(k).and_then(Value::as_u64).unwrap_or(0) as u32
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
                let version = u(ev, "version");
                if let Some(def) = self.defs.get_mut(&name) {
                    if let Some(v) = def.get_mut(version) {
                        v.status = "kept".into();
                    }
                    def.soak_uses = 0;
                }
                self.push_verdict(ev, "kept");
            }
            "def_reverted" => {
                let version = u(ev, "version");
                if let Some(def) = self.defs.get_mut(&name) {
                    if let Some(v) = def.get_mut(version) {
                        v.status = "reverted".into();
                    }
                    def.live = ev.get("restored").and_then(Value::as_u64).map(|r| r as u32);
                    def.soak_uses = 0;
                }
                self.push_verdict(ev, "reverted");
            }
            "lesson_added" => self.lessons.push(Lesson {
                id: ev.get("id").and_then(Value::as_i64).unwrap_or(0),
                text: s(ev, "text"),
                score: 0,
                retired: false,
                origin: s(ev, "origin"),
            }),
            "lesson_scored" => {
                let id = ev.get("id").and_then(Value::as_i64).unwrap_or(0);
                if let Some(l) = self.lessons.iter_mut().find(|l| l.id == id) {
                    l.score = ev.get("score").and_then(Value::as_i64).unwrap_or(l.score);
                }
            }
            "lesson_retired" => {
                let id = ev.get("id").and_then(Value::as_i64).unwrap_or(0);
                if let Some(l) = self.lessons.iter_mut().find(|l| l.id == id) {
                    l.retired = true;
                }
            }
            "task_started" => self.tasks.push(TaskRecord {
                id: ev.get("task_id").and_then(Value::as_u64).unwrap_or(0),
                task: s(ev, "task"),
                success: None,
                summary: String::new(),
            }),
            "task_finished" => {
                let id = ev.get("task_id").and_then(Value::as_u64).unwrap_or(0);
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
}

pub fn words(s: &str, min: usize) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= min)
        .map(str::to_lowercase)
        .collect()
}
