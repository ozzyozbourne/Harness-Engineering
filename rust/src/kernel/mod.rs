//! The kernel ring: agent loop, gate, knowledge, journal replay.
//! Immutable to the agent (`write_file` refuses this directory).

pub mod cli;
pub mod journal;
pub mod provider;
pub mod state;
pub mod tools;
pub mod userspace;
#[cfg(test)]
mod provider_tests;

// The seed hooks double as the kernel's fallbacks when a hook fails, so the
// default behavior exists in exactly one place.
#[path = "../../seed/compose_system.rs"]
mod seed_compose_system;
#[path = "../../seed/select_lessons.rs"]
mod seed_select_lessons;

use journal::Journal;
use provider::{ModelRequest, Provider};
use serde_json::{Value, json};
use state::{DefVersion, State, words};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use userspace::Userspace;

const BASE_PROMPT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../base-prompt.md");
const BASE_PROMPT_EMBEDDED: &str = include_str!("../../../base-prompt.md");
const KERNEL_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

pub const REFINE_PROMPT: &str = "Refine yourself. Review the recent task outcomes below, the verdicts in your system prompt, \
and your current userspace (use list_defs, show_def and recall). Then improve userspace through propose_def or knowledge \
through remember. Make at most 3 changes, prefer small ones, and give every tool or hook real tests. \
Finish with a short summary of what you changed and why.";

/// Seed userspace, installed as kept v1 when the journal is empty.
struct Seed {
    name: &'static str,
    kind: &'static str,
    source: &'static str,
    description: &'static str,
    tests: &'static str,
    input_schema: &'static str,
}

const SEEDS: [Seed; 3] = [
    Seed {
        name: "compose_system",
        kind: "hook",
        source: include_str!("../../seed/compose_system.rs"),
        description: "Joins notes under a Notes heading and lessons under a Lessons heading.",
        tests: r###"[{"input": {"notes": ["Be brief."], "lessons": ["Run tests first."]}, "expect": "## Lessons\n- Run tests first."}]"###,
        input_schema: "null",
    },
    Seed {
        name: "select_lessons",
        kind: "hook",
        source: include_str!("../../seed/select_lessons.rs"),
        description: "Top 8 lessons by score sharing a 4+ letter word with the task, else top 3 by score.",
        tests: r#"[{"input": {"task": "fix the parser bug", "lessons": [{"id": 1, "text": "the parser needs tests", "score": 0}, {"id": 2, "text": "unrelated", "score": 5}]}, "expect": "[1]"}]"#,
        input_schema: "null",
    },
    Seed {
        name: "word_count",
        kind: "tool",
        source: include_str!("../../seed/word_count.rs"),
        description: "Counts whitespace-separated words in `text`.",
        tests: r#"[{"input": {"text": "one two three"}, "expect": "3"}]"#,
        input_schema: r#"{"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}"#,
    },
];

pub struct Config {
    pub state_dir: PathBuf,
    pub work_dir: PathBuf,
    pub soak: u32,
    pub reflect: bool,
    /// Shared CARGO_TARGET_DIR for generated crates (default `<state>/target`,
    /// or `$CMH_TARGET_DIR`).
    pub target_dir: Option<PathBuf>,
    pub call_timeout: Duration,
    pub max_turns: usize,
    /// Print assistant text to stdout as it arrives (repl). When false it goes
    /// to the stderr trace and only the caller prints the final answer.
    pub echo_text: bool,
}

impl Config {
    pub fn new(state_dir: impl Into<PathBuf>) -> Config {
        Config {
            state_dir: state_dir.into(),
            work_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            soak: 3,
            reflect: true,
            target_dir: std::env::var_os("CMH_TARGET_DIR").map(PathBuf::from),
            call_timeout: Duration::from_secs(15),
            max_turns: 40,
            echo_text: true,
        }
    }
}

pub struct TaskOutcome {
    pub success: bool,
    pub answer: String,
    pub error: Option<String>,
}

pub struct Kernel {
    pub cfg: Config,
    pub state: State,
    journal: Journal,
    userspace: Userspace,
    provider: Box<dyn Provider>,
    base_prompt: String,
    /// Paths `write_file` refuses: the state dir, the kernel source, the base prompt.
    protected: Vec<PathBuf>,
}

pub fn trace(line: &str) {
    eprintln!("{line}");
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() > max { format!("{}...", line.chars().take(max).collect::<String>()) } else { line.to_string() }
}

fn compact(v: &Value, max: usize) -> String {
    first_line(&v.to_string(), max)
}

pub fn base_prompt() -> String {
    std::fs::read_to_string(BASE_PROMPT_PATH).unwrap_or_else(|_| BASE_PROMPT_EMBEDDED.to_string())
}

impl Kernel {
    /// Opens the state dir and replays the journal (installing the seed
    /// userspace if the journal is empty).
    pub fn open(cfg: Config, provider: Box<dyn Provider>) -> Result<Kernel, String> {
        let state_dir = std::path::absolute(&cfg.state_dir).map_err(|e| e.to_string())?;
        let journal = Journal::open(&state_dir)?;
        let target_dir = match &cfg.target_dir {
            Some(t) => std::path::absolute(t).map_err(|e| e.to_string())?,
            None => state_dir.join("target"),
        };
        let protected = vec![
            state_dir.clone(),
            tools::resolve(&cfg.work_dir, KERNEL_SRC),
            tools::resolve(&cfg.work_dir, BASE_PROMPT_PATH),
        ];
        let mut cfg = cfg;
        cfg.state_dir = state_dir.clone();
        let mut k = Kernel {
            userspace: Userspace::new(&state_dir, target_dir),
            cfg,
            state: State::default(),
            journal,
            provider,
            base_prompt: base_prompt(),
            protected,
        };
        let events = k.journal.read_all()?;
        for ev in &events {
            k.state.apply(ev);
        }
        if events.is_empty() {
            for s in &SEEDS {
                k.record("def_trial", json!({
                    "name": s.name, "version": 1, "kind": s.kind, "source": s.source,
                    "description": s.description, "origin": "seed",
                    "tests": serde_json::from_str::<Value>(s.tests).expect("seed tests"),
                    "input_schema": serde_json::from_str::<Value>(s.input_schema).expect("seed schema"),
                }))?;
                k.record("def_kept", json!({"name": s.name, "version": 1, "seed": true, "verdict": ""}))?;
            }
        }
        Ok(k)
    }

    fn record(&mut self, kind: &str, fields: Value) -> Result<Value, String> {
        let ev = self.journal.append(kind, fields)?;
        self.state.apply(&ev);
        Ok(ev)
    }

    fn rec(&mut self, kind: &str, fields: Value) {
        if let Err(e) = self.record(kind, fields) {
            trace(&format!("[journal] write failed: {e}"));
        }
    }

    // ----------------------------------------------------------------- gate

    /// Runs the gate on a proposed version: static checks, then (for compiled
    /// kinds) a scratch build and the tests.
    fn validate(&mut self, name: &str, rec: &DefVersion) -> Result<(), String> {
        self.check_shape(name, rec)?;
        if rec.kind == "note" {
            return Ok(());
        }
        self.run_gate_tests(name, rec)
    }

    /// The checks that need no compiler: name, kind, source, schema, tests.
    fn check_shape(&self, name: &str, rec: &DefVersion) -> Result<(), String> {
        let kind = rec.kind.as_str();
        let valid_name = name.len() <= 48
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid_name {
            return Err(format!("invalid name `{name}`: use [a-z][a-z0-9_]*, at most 48 chars"));
        }
        if tools::KERNEL_TOOLS.contains(&name) {
            return Err(format!("`{name}` is a kernel tool and cannot be redefined"));
        }
        if !["tool", "note", "hook"].contains(&kind) {
            return Err(format!("unknown kind `{kind}` (tool, note, hook)"));
        }
        if let Some(prev) = self.state.defs.get(name).and_then(|d| d.versions.last())
            && prev.kind != kind
        {
            return Err(format!("`{name}` is a {}; it cannot become a {kind}", prev.kind));
        }
        if rec.source.trim().is_empty() {
            return Err("source is empty".into());
        }
        if kind == "note" {
            return Ok(());
        }
        userspace::entry_kind(kind, name)?;
        if kind == "tool" && rec.input_schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err("tools need an input_schema JSON Schema object with \"type\": \"object\"".into());
        }
        let tests = rec.tests.as_array().filter(|t| !t.is_empty()).ok_or("tools and hooks need at least one test")?;
        for (i, t) in tests.iter().enumerate() {
            if t.get("expect").and_then(Value::as_str).is_none() || !t.get("input").is_some_and(Value::is_object) {
                return Err(format!("test {}: needs an object `input` and a string `expect`", i + 1));
            }
        }
        Ok(())
    }

    /// Scratch: compile into its own library, load it, run the tests. The
    /// live registry is not touched.
    fn run_gate_tests(&mut self, name: &str, rec: &DefVersion) -> Result<(), String> {
        let version = rec.version;
        let (build_time, lib) = self.userspace.load(name, version, &rec.kind, &rec.source);
        if let Some(t) = build_time {
            trace(&format!("[gate] cargo build {name} v{version}: {:.1}s", t.as_secs_f64()));
        }
        let lib = lib?;
        let tests = rec.tests.as_array().map(Vec::as_slice).unwrap_or_default();
        for (i, t) in tests.iter().enumerate() {
            let expect = t["expect"].as_str().unwrap_or_default();
            match lib.call(&t["input"], self.cfg.call_timeout) {
                Ok(out) if out.contains(expect) => {}
                Ok(out) => {
                    return Err(format!(
                        "test {} failed: input {} expected substring {expect:?}, got {:?}",
                        i + 1, compact(&t["input"], 300), tools::truncate(&out, 500)
                    ));
                }
                Err(e) => return Err(format!("test {} failed: input {}: {e}", i + 1, compact(&t["input"], 300))),
            }
        }
        Ok(())
    }

    /// The gate. Returns the verdict text for the model, `Err` if rejected.
    pub fn propose_def(&mut self, input: &Value) -> Result<String, String> {
        let (name, kind, source, description) =
            (state::s(input, "name"), state::s(input, "kind"), state::s(input, "source"), state::s(input, "description"));
        let tests = input.get("tests").cloned().unwrap_or(json!([]));
        let schema = if kind == "tool" { input.get("input_schema").cloned().unwrap_or(Value::Null) } else { Value::Null };
        self.rec("def_proposed", json!({"name": name, "kind": kind, "source": source,
            "description": description, "tests": tests, "input_schema": schema}));
        let version = self.state.defs.get(&name).map_or(1, |d| d.next_version());
        let rec = DefVersion { version, kind, source, description, tests, input_schema: schema, status: "trial".into() };
        self.install_trial(&name, rec, "propose")
    }

    pub fn rollback_def(&mut self, name: &str, version: u32) -> Result<String, String> {
        let def = self.state.defs.get(name).ok_or(format!("no definition named `{name}`"))?;
        let old = def.get(version).ok_or(format!("`{name}` has no version {version}"))?.clone();
        let rec = DefVersion { version: def.next_version(), status: "trial".into(), ..old };
        self.install_trial(name, rec, &format!("rollback to v{version}"))
    }

    fn install_trial(&mut self, name: &str, rec: DefVersion, origin: &str) -> Result<String, String> {
        let v = rec.version;
        if let Err(e) = self.validate(name, &rec) {
            self.rec("def_rejected", json!({"name": name, "version": v, "error": e}));
            trace(&format!("[gate] {name} v{v} rejected: {}", first_line(&e, 120)));
            return Err(format!("rejected: {e}\nThe live version is unchanged."));
        }
        // A trial that is still soaking is superseded (reverted) first.
        if let Some(live) = self.state.live(name).filter(|l| l.status == "trial") {
            let (lv, desc) = (live.version, live.description.clone());
            let verdict = format!("def {name} v{lv} ({desc:?}) reverted: superseded by v{v} before its soak finished");
            self.revert(name, lv, "superseded", verdict);
        }
        self.rec("def_trial", json!({"name": name, "version": v, "kind": rec.kind, "source": rec.source,
            "description": rec.description, "tests": rec.tests, "input_schema": rec.input_schema, "origin": origin}));
        let soak = self.cfg.soak;
        Ok(match rec.kind.as_str() {
            "note" => format!("trial v{v}: note `{name}` is live; kept after {soak} tasks that do not fail."),
            _ => format!("trial v{v}: compiled and passed {} test(s); `{name}` is live now and is kept after {soak} clean uses (any error reverts it).",
                rec.tests.as_array().map_or(0, Vec::len)),
        })
    }

    /// Ends a trial: the live pointer goes back to the most recent kept
    /// version (none if there is none). Returns that version.
    fn revert(&mut self, name: &str, version: u32, error: &str, verdict: String) -> Option<u32> {
        let restored = self.state.defs.get(name).and_then(|d| d.last_kept());
        self.rec("def_reverted", json!({"name": name, "version": version, "restored": restored, "error": error, "verdict": verdict}));
        restored
    }

    /// Records one use of a live definition and applies the soak rules.
    fn record_use(&mut self, name: &str, version: u32, error: Option<&str>, input: &Value) {
        self.rec("def_use", json!({"name": name, "version": version, "ok": error.is_none(), "error": error}));
        let Some(def) = self.state.defs.get(name) else { return };
        let Some(live) = def.live_version().filter(|l| l.version == version && l.status == "trial") else { return };
        let desc = live.description.clone();
        if let Some(err) = error {
            let verdict = format!("def {name} v{version} ({desc:?}) reverted: {} on input {}", first_line(err, 200), compact(input, 200));
            let restored = self.revert(name, version, err, verdict);
            let now = restored.map_or("definition removed".into(), |r| format!("live: v{r}"));
            trace(&format!("[gate] {name} v{version} reverted: {} ({now})", first_line(err, 100)));
        } else if def.soak_uses >= self.cfg.soak {
            let verdict = format!("def {name} v{version} ({desc:?}) kept after {} clean uses", def.soak_uses);
            self.rec("def_kept", json!({"name": name, "version": version, "verdict": verdict}));
            trace(&format!("[gate] {name} v{version} kept"));
        }
    }

    /// Late binding: resolves `name` in the registry on every call, so a new
    /// version (or a revert) takes effect on the very next call, mid-task,
    /// without restarting the process.
    pub fn call_def(&mut self, name: &str, input: &Value) -> Result<String, String> {
        let Some(live) = self.state.live(name) else {
            return Err(format!("no live definition `{name}`"));
        };
        let version = live.version;
        let (_, lib) = self.userspace.load(name, version, &live.kind, &live.source);
        let result = lib.and_then(|lib| lib.call(input, self.cfg.call_timeout));
        self.record_use(name, version, result.as_ref().err().map(String::as_str), input);
        result
    }

    // ------------------------------------------------------------ prompting

    fn select_lessons(&mut self, task: &str) -> Vec<i64> {
        let lessons: Vec<Value> =
            self.state.active_lessons().map(|l| json!({"id": l.id, "text": l.text, "score": l.score})).collect();
        let ids = match self.call_def("select_lessons", &json!({"task": task, "lessons": lessons})) {
            // The generated wrapper serializes a `Vec<i64>`, so this parses.
            Ok(out) => serde_json::from_str::<Vec<i64>>(&out).unwrap_or_default(),
            Err(e) => {
                trace(&format!("[hook] select_lessons failed, using default: {}", first_line(&e, 100)));
                seed_select_lessons::select_lessons(task, &lessons)
            }
        };
        // Keep only active lessons, each once, in the hook's order.
        let active: HashSet<i64> = self.state.active_lessons().map(|l| l.id).collect();
        let mut seen = HashSet::new();
        ids.into_iter().filter(|id| active.contains(id) && seen.insert(*id)).collect()
    }

    /// System blocks: the immutable base prompt, then the supplemental prompt
    /// built by the `compose_system` hook plus the verdicts (which bypass
    /// `select_lessons` and cannot be dropped by a hook).
    pub fn system_prompt(&mut self, selected: &[i64]) -> Vec<String> {
        let notes: Vec<String> = self.state.live_defs("note").map(|(_, v)| v.source.clone()).collect();
        let lessons: Vec<String> = selected.iter().filter_map(|id| self.state.lesson(*id)).map(|l| l.text.clone()).collect();
        let mut supplemental = match self.call_def("compose_system", &json!({"notes": notes, "lessons": lessons})) {
            Ok(s) => s,
            Err(e) => {
                trace(&format!("[hook] compose_system failed, using default: {}", first_line(&e, 100)));
                seed_compose_system::compose_system(&notes, &lessons)
            }
        };
        let verdicts: Vec<&str> = self.state.verdicts.iter().rev().take(20).map(|v| v.text.as_str()).collect();
        if !verdicts.is_empty() {
            supplemental.push_str("\n\n## Verdicts on past userspace changes\n");
            for v in verdicts.iter().rev() {
                supplemental.push_str(&format!("- {v}\n"));
            }
        }
        vec![self.base_prompt.clone(), supplemental.trim().to_string()]
    }

    pub fn tool_list(&self) -> Vec<Value> {
        let mut list = tools::schemas();
        for (def, v) in self.state.live_defs("tool") {
            list.push(json!({"name": def.name, "description": v.description, "input_schema": v.input_schema}));
        }
        list
    }

    // ---------------------------------------------------------------- tools

    fn exec_tool(&mut self, name: &str, input: &Value) -> Result<String, String> {
        let wd = &self.cfg.work_dir;
        match name {
            "bash" => tools::bash(wd, input),
            "read_file" => tools::read_file(wd, input),
            "write_file" => tools::write_file(wd, input, &self.protected),
            "list_defs" => Ok(self.defs_table()),
            "show_def" => self.show_def(&state::s(input, "name"), state::opt_u(input, "version")),
            "propose_def" => self.propose_def(input),
            "rollback_def" => match state::opt_u(input, "version") {
                Some(v) => self.rollback_def(&state::s(input, "name"), v),
                None => Err("`version` (integer) is required".into()),
            },
            "remember" => self.remember(&state::s(input, "text"), "remember"),
            "recall" => Ok(self.recall(&state::s(input, "query"))),
            _ if self.state.live(name).is_some_and(|v| v.kind == "tool") => {
                self.call_def(name, input).map(|s| tools::truncate(&s, 20 * 1024))
            }
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    pub fn defs_table(&self) -> String {
        let mut out = String::new();
        for def in self.state.defs.values() {
            let (ver, status, kind, desc) = match def.live_version() {
                Some(v) => (format!("v{}", v.version), v.status.clone(), v.kind.clone(), v.description.clone()),
                None => ("-".into(), "removed".into(), def.versions.last().map_or(String::new(), |v| v.kind.clone()), String::new()),
            };
            let soak = if status == "trial" { format!(" ({}/{} clean uses)", def.soak_uses, self.cfg.soak) } else { String::new() };
            out.push_str(&format!("{:<16} {:<5} {:<4} {}{soak}  {desc}\n", def.name, kind, ver, status));
        }
        out
    }

    pub fn show_def(&self, name: &str, version: Option<u32>) -> Result<String, String> {
        let def = self.state.defs.get(name).ok_or(format!("no definition named `{name}`"))?;
        let v = match version {
            Some(n) => def.get(n),
            None => def.live_version().or(def.versions.last()),
        }
        .ok_or(format!("`{name}` has no such version"))?;
        let mut rec = serde_json::to_value(v).map_err(|e| e.to_string())?;
        rec["name"] = json!(name);
        rec["live"] = json!(def.live == Some(v.version));
        rec["all_versions"] = json!(def.versions.iter().map(|x| format!("v{} {}", x.version, x.status)).collect::<Vec<_>>());
        serde_json::to_string_pretty(&rec).map_err(|e| e.to_string())
    }

    pub fn remember(&mut self, text: &str, origin: &str) -> Result<String, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("`text` is required".into());
        }
        if self.state.has_lesson_text(text) {
            return Ok("already known".into());
        }
        let id = self.state.next_lesson_id();
        self.record("lesson_added", json!({"id": id, "text": text, "origin": origin}))?;
        Ok(format!("lesson #{id} recorded"))
    }

    pub fn recall(&self, query: &str) -> String {
        let q = words(query, 3);
        let mut hits: Vec<(i64, String)> = self
            .state
            .active_lessons()
            .filter(|l| !words(&l.text, 3).is_disjoint(&q))
            .map(|l| (l.score, format!("lesson #{} (score {}): {}", l.id, l.score, l.text)))
            .chain(self.state.verdicts.iter().filter(|v| !words(&v.text, 3).is_disjoint(&q)).map(|v| (0, format!("verdict: {}", v.text))))
            .collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        if hits.is_empty() { "nothing relevant".into() } else { hits.into_iter().map(|h| h.1).collect::<Vec<_>>().join("\n") }
    }

    pub fn knowledge_text(&self) -> String {
        let mut out = String::from("Lessons:\n");
        for l in &self.state.lessons {
            let retired = if l.retired { " [retired]" } else { "" };
            out.push_str(&format!("  #{:<3} score {:>3}{retired} ({}) {}\n", l.id, l.score, l.origin, l.text));
        }
        out.push_str("Verdicts:\n");
        for v in &self.state.verdicts {
            out.push_str(&format!("  {}\n", v.text));
        }
        out
    }

    // ----------------------------------------------------------- agent loop

    /// One task: the model/tool loop, then the bookkeeping that learns from
    /// its outcome (note soak, lesson scores, reflection).
    pub fn run_task(&mut self, task: &str) -> TaskOutcome {
        let task_id = self.state.tasks.len() as u64 + 1;
        self.rec("task_started", json!({"task_id": task_id, "task": task}));
        let selected = self.select_lessons(task);
        let mut messages = vec![json!({"role": "user", "content": task})];
        let mut tool_errors: Vec<bool> = Vec::new();
        let mut transcript = vec![format!("TASK: {task}")];
        let (mut kernel_error, mut stop, mut answer) = (None::<String>, String::new(), String::new());
        for turn in 0..self.cfg.max_turns {
            let req = ModelRequest { system: self.system_prompt(&selected), tools: self.tool_list(), messages: messages.clone() };
            let resp = match self.provider.complete(&req) {
                Ok(r) => r,
                Err(e) => {
                    trace(&format!("[kernel] model request failed: {}", first_line(&e, 200)));
                    kernel_error = Some(e);
                    break;
                }
            };
            let text: Vec<&str> = resp.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let text = text.join("\n").trim().to_string();
            if !text.is_empty() {
                if self.cfg.echo_text {
                    println!("{text}");
                } else {
                    trace(&format!("[assistant] {}", first_line(&text, 160)));
                }
                transcript.push(format!("ASSISTANT: {}", tools::truncate(&text, 400)));
                answer = text;
            }
            stop = resp.stop_reason.clone();
            messages.push(json!({"role": "assistant", "content": resp.content}));
            if stop != "tool_use" {
                break;
            }
            let results = self.run_tool_calls(&resp.content, &mut transcript, &mut tool_errors);
            if results.is_empty() {
                kernel_error = Some("stop_reason tool_use without tool_use blocks".into());
                break;
            }
            messages.push(json!({"role": "user", "content": results}));
            if turn + 1 == self.cfg.max_turns {
                kernel_error = Some(format!("turn limit ({}) reached", self.cfg.max_turns));
            }
        }
        let tail_ok = tool_errors.iter().rev().take(2).all(|e| !e);
        let success = kernel_error.is_none() && stop == "end_turn" && tail_ok;

        self.soak_trial_notes(task_id, task, success);
        self.score_lessons(task_id, &selected, success);
        let summary = first_line(&answer, 200);
        self.rec("task_finished", json!({"task_id": task_id, "success": success, "error": kernel_error, "summary": summary}));
        trace(&format!("[task] {task_id} {}", if success { "succeeded" } else { "failed" }));
        transcript.push(format!("OUTCOME: {}", if success { "success" } else { "failure" }));
        if self.cfg.reflect {
            self.reflect(&transcript.join("\n"));
        }
        TaskOutcome { success, answer, error: kernel_error }
    }

    /// Runs every `tool_use` block of one assistant turn and returns the
    /// `tool_result` blocks for the next user message.
    fn run_tool_calls(&mut self, content: &[Value], transcript: &mut Vec<String>, tool_errors: &mut Vec<bool>) -> Vec<Value> {
        let mut results = Vec::new();
        for b in content.iter().filter(|b| b["type"] == "tool_use") {
            let name = b["name"].as_str().unwrap_or("");
            let input = b.get("input").cloned().unwrap_or(json!({}));
            let result = self.exec_tool(name, &input);
            let (out, is_err) = match &result {
                Ok(s) => (s.clone(), false),
                Err(e) => (format!("error: {e}"), true),
            };
            trace(&tool_trace(name, &input, &result));
            transcript.push(format!("TOOL {name} {} -> {}{}", compact(&input, 150), if is_err { "ERROR " } else { "" }, first_line(&out, 150)));
            tool_errors.push(is_err);
            results.push(json!({"type": "tool_result", "tool_use_id": b["id"], "content": out, "is_error": is_err}));
        }
        results
    }

    /// Notes soak over tasks: each finished task is one use of every trial
    /// note, and a task that does not succeed counts as an error.
    fn soak_trial_notes(&mut self, task_id: u64, task: &str, success: bool) {
        let trial_notes: Vec<(String, u32)> = self
            .state
            .live_defs("note")
            .filter(|(_, v)| v.status == "trial")
            .map(|(d, v)| (d.name.clone(), v.version))
            .collect();
        let error = (!success).then(|| format!("task {task_id} failed"));
        for (name, v) in trial_notes {
            self.record_use(&name, v, error.as_deref(), &json!({"task": task}));
        }
    }

    /// Scores the lessons that were in the prompt by the task outcome and
    /// retires those that fall to -3.
    fn score_lessons(&mut self, task_id: u64, selected: &[i64], success: bool) {
        let delta = if success { 1 } else { -1 };
        for id in selected {
            let Some(l) = self.state.lesson(*id) else { continue };
            let score = l.score + delta;
            self.rec("lesson_scored", json!({"id": id, "delta": delta, "score": score, "task_id": task_id}));
            if score <= -3 {
                self.rec("lesson_retired", json!({"id": id}));
                trace(&format!("[learn] lesson #{id} retired (score {score})"));
            }
        }
    }

    fn reflect(&mut self, transcript: &str) {
        let prompt = format!(
            "Reflect on this finished task.\n\n{}\n\nReply with only a JSON array of 0 to 3 short, reusable lessons \
             (strings) that would help with future tasks. Skip task-specific trivia; reply [] if there is nothing durable.",
            tools::truncate(transcript, 12_000)
        );
        let req = ModelRequest { system: vec![self.base_prompt.clone()], tools: vec![], messages: vec![json!({"role": "user", "content": prompt})] };
        let resp = match self.provider.complete(&req) {
            Ok(r) => r,
            Err(e) => {
                trace(&format!("[reflect] failed: {}", first_line(&e, 160)));
                return;
            }
        };
        let text: String = resp.content.iter().filter_map(|b| b["text"].as_str()).collect();
        let parsed = text
            .find('[')
            .zip(text.rfind(']'))
            .and_then(|(a, b)| serde_json::from_str::<Vec<String>>(&text[a..=b]).ok());
        let Some(lessons) = parsed else {
            trace("[reflect] no JSON array in reply");
            return;
        };
        let mut added = 0;
        for l in lessons.iter().take(3) {
            if matches!(self.remember(l, "reflection"), Ok(m) if m.starts_with("lesson")) {
                added += 1;
            }
        }
        trace(&format!("[reflect] +{added} lesson(s)"));
    }

    pub fn refine(&mut self) -> TaskOutcome {
        let recent: Vec<String> = self
            .state
            .tasks
            .iter()
            .rev()
            .take(10)
            .map(|t| {
                let outcome = match t.success { Some(true) => "success", Some(false) => "failure", None => "unfinished" };
                format!("- task {} [{outcome}]: {} => {}", t.id, first_line(&t.task, 120), t.summary)
            })
            .collect();
        let recent = if recent.is_empty() { "(no tasks yet)".into() } else { recent.into_iter().rev().collect::<Vec<_>>().join("\n") };
        self.run_task(&format!("{REFINE_PROMPT}\n\nRecent task outcomes:\n{recent}"))
    }
}

fn tool_trace(name: &str, input: &Value, result: &Result<String, String>) -> String {
    let arg = match name {
        "bash" => first_line(input["command"].as_str().unwrap_or(""), 60),
        "propose_def" | "show_def" | "rollback_def" => {
            let v = input.get("version").map(|v| format!(" {v}")).unwrap_or_default();
            format!("{}{v}", input["name"].as_str().unwrap_or("?"))
        }
        "read_file" | "write_file" => input["path"].as_str().unwrap_or("").to_string(),
        "remember" | "recall" => first_line(input["text"].as_str().or(input["query"].as_str()).unwrap_or(""), 60),
        _ => compact(input, 60),
    };
    let res = match (name, result) {
        ("propose_def" | "rollback_def", Ok(s)) => first_line(s, 60).split(':').next().unwrap_or("").to_string(),
        ("propose_def" | "rollback_def", Err(_)) => "rejected".into(),
        ("bash", Ok(_)) => "exit 0".into(),
        ("bash", Err(e)) => e.lines().last().unwrap_or("").trim_matches(['[', ']']).to_string(),
        (_, Ok(s)) => first_line(s, 60),
        (_, Err(e)) => format!("error: {}", first_line(e, 80)),
    };
    format!("[tool] {name} {arg} -> {res}")
}
