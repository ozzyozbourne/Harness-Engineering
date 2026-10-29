//! The kernel ring: agent loop, gate, knowledge, journal replay, and the
//! WebAssembly wall around the Forth userspace.
//! Immutable to the agent (`write_file` and `file-write` refuse this crate).

pub mod cli;
pub mod forth;
pub mod journal;
pub mod provider;
pub mod state;
pub mod tools;
#[cfg(test)]
mod provider_tests;

use journal::Journal;
use provider::{ModelRequest, Provider};
use serde_json::{Value, json};
use state::{DefVersion, State, words};
use std::path::{Path, PathBuf};
use forth::{ForthVm, Limits, Live};
use std::time::Instant;

const BASE_PROMPT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../base-prompt.md");
const BASE_PROMPT_EMBEDDED: &str = include_str!("../../../../base-prompt.md");
/// The host crate (kernel source, build script, tests): never writable by the agent.
const KERNEL_SRC: &str = env!("CARGO_MANIFEST_DIR");
/// Kernel-owned system block after the base prompt: the Forth dialect.
pub const FORTH_REFERENCE: &str = include_str!("forth_reference.md");
/// Definitions compiled eagerly at startup; everything else loads lazily.
const SEED_NAMES: [&str; 3] = ["compose_system", "select_lessons", "word_count"];

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
        source: include_str!("../../../seed/compose_system.fs"),
        description: "Joins notes under a Notes heading and lessons under a Lessons heading.",
        tests: r###"[{"input": {"notes": ["Be brief."], "lessons": ["Run tests first."]}, "expect": "## Lessons\n- Run tests first."}]"###,
        input_schema: "null",
    },
    Seed {
        name: "select_lessons",
        kind: "hook",
        source: include_str!("../../../seed/select_lessons.fs"),
        description: "Top 8 lessons by score sharing a 4+ letter word with the task, else top 3 by score.",
        tests: r#"[{"input": {"task": "fix the parser bug", "lessons": [{"id": 1, "text": "the parser needs tests", "score": 0}, {"id": 2, "text": "unrelated", "score": 5}]}, "expect": "[1]"}]"#,
        input_schema: "null",
    },
    Seed {
        name: "word_count",
        kind: "tool",
        source: include_str!("../../../seed/word_count.fs"),
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
    /// Fuel, wall-clock and memory limits for every entry into the Forth VM.
    pub limits: Limits,
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
            limits: Limits::default(),
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
    /// The live Forth image. Validation never touches it.
    vm: ForthVm,
    /// Definitions compiled lazily so far in this process (name, version).
    pub lazy_loads: Vec<(String, u32)>,
    provider: Box<dyn Provider>,
    base_prompt: String,
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
        let mut cfg = cfg;
        cfg.state_dir = state_dir.clone();
        let vm = ForthVm::new(cfg.limits.clone(), cfg.work_dir.clone(), protected_paths(&cfg))?;
        let mut k = Kernel {
            vm,
            lazy_loads: Vec::new(),
            cfg,
            state: State::default(),
            journal,
            provider,
            base_prompt: base_prompt(),
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
        k.load_seeds();
        Ok(k)
    }

    /// Compiles the live versions of the seed-named definitions into the
    /// live VM. Every other definition is compiled on its first call.
    fn load_seeds(&mut self) {
        let code = self.live_code();
        for name in SEED_NAMES {
            if let Some((v, src)) = code.get(name)
                && let Err(e) = self.vm.define(name, *v, src, &code)
            {
                trace(&format!("[forth] could not load {name} v{v}: {}", first_line(&e, 160)));
            }
        }
    }

    /// name -> (live version, source) for every compiled kind.
    fn live_code(&self) -> Live {
        self.state
            .defs
            .values()
            .filter_map(|d| d.live_version().filter(|v| v.kind != "note").map(|v| (d.name.clone(), (v.version, v.source.clone()))))
            .collect()
    }

    /// Makes the VM's public slot for `name` match the registry: bound to
    /// `version` if that version is compiled, otherwise unbound (lazy).
    fn sync_slot(&mut self, name: &str, version: Option<u32>) {
        match version {
            Some(v) if self.vm.bound(name) == v => {}
            Some(v) if self.vm.bind(name, v) => {}
            _ => self.vm.unbind(name),
        }
    }

    /// Version currently bound in the live VM (0 = not compiled yet).
    pub fn vm_bound(&mut self, name: &str) -> u32 {
        self.vm.bound(name)
    }

    /// (data space bytes used, compiled words) in the live VM.
    pub fn vm_stats(&mut self) -> (u64, u64) {
        self.vm.stats()
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

    fn validate(&mut self, name: &str, kind: &str, version: u32, source: &str, tests: &Value, schema: &Value) -> Result<usize, String> {
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
        if source.trim().is_empty() {
            return Err("source is empty".into());
        }
        if kind == "note" {
            return Ok(0);
        }
        if kind == "hook" && !["compose_system", "select_lessons"].contains(&name) {
            return Err(format!("unknown hook `{name}`: hooks must be named compose_system or select_lessons"));
        }
        if kind == "tool" && schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err("tools need an input_schema JSON Schema object with \"type\": \"object\"".into());
        }
        let tests = tests.as_array().filter(|t| !t.is_empty()).ok_or("tools and hooks need at least one test")?;
        for (i, t) in tests.iter().enumerate() {
            if t.get("expect").and_then(Value::as_str).is_none() || !t.get("input").is_some_and(Value::is_object) {
                return Err(format!("test {}: needs an object `input` and a string `expect`", i + 1));
            }
        }
        // Scratch: a brand-new wasm instance with its own store and fuel. The
        // live VM is not touched.
        let t0 = Instant::now();
        let code = self.live_code();
        let mut scratch = ForthVm::new(self.cfg.limits.clone(), self.cfg.work_dir.clone(), protected_paths(&self.cfg))?;
        scratch.define(name, version, source, &code).map_err(|e| format!("compile failed:\n{e}"))?;
        let compiled = scratch.snapshot();
        for (i, t) in tests.iter().enumerate() {
            let expect = t["expect"].as_str().unwrap_or_default();
            let input = t["input"].to_string();
            let out = scratch.call(name, input.as_bytes(), &code);
            if out.trapped {
                scratch.restore(&compiled)?;
            }
            let result = out.result.and_then(|o| check_output(name, &o).map(|()| o));
            match result {
                Ok(out) if out.contains(expect) => {}
                Ok(out) => {
                    return Err(format!(
                        "test {} failed: input {} expected substring {expect:?}, got {:?}",
                        i + 1, compact(&t["input"], 300), tools::truncate(&out, 500)
                    ));
                }
                Err(e) => return Err(format!("test {} failed:\n{e}", i + 1)),
            }
        }
        trace(&format!("[gate] scratch vm: {name} v{version} compiled, {} test(s) passed in {} ms", tests.len(), t0.elapsed().as_millis()));
        Ok(tests.len())
    }

    /// Scratchpad: compiles and runs a snippet in a throwaway VM. Nothing is
    /// journaled, so experiments do not count as proposals.
    fn forth_eval(&mut self, input: &Value) -> Result<String, String> {
        let source = input.get("source").and_then(Value::as_str).unwrap_or("");
        if source.trim().is_empty() {
            return Err("source is empty".into());
        }
        let arg = input.get("input").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        let code = self.live_code();
        let mut vm = ForthVm::new(self.cfg.limits.clone(), self.cfg.work_dir.clone(), protected_paths(&self.cfg))?;
        vm.define("scratchpad", 1, source, &code).map_err(|e| format!("compile failed:\n{e}"))?;
        let out = vm.call("scratchpad", arg.to_string().as_bytes(), &code);
        out.result.map(|o| format!("output: {}", tools::truncate(&o, 20 * 1024)))
    }

    /// The gate. Returns the verdict text for the model, `Err` if rejected.
    pub fn propose_def(&mut self, input: &Value) -> Result<String, String> {
        let name = input.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let kind = input.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
        let source = input.get("source").and_then(Value::as_str).unwrap_or("").to_string();
        let description = input.get("description").and_then(Value::as_str).unwrap_or("").to_string();
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
        if let Err(e) = self.validate(name, &rec.kind, v, &rec.source, &rec.tests, &rec.input_schema) {
            self.rec("def_rejected", json!({"name": name, "version": v, "error": e}));
            trace(&format!("[gate] {name} v{v} rejected: {}", first_line(&e, 120)));
            return Err(format!("rejected: {e}\nThe live version is unchanged."));
        }
        // A trial that is still soaking is superseded (reverted) first.
        if let Some(def) = self.state.defs.get(name)
            && let Some(live) = def.live_version()
            && live.status == "trial"
        {
            let (lv, desc, restored) = (live.version, live.description.clone(), def.last_kept());
            let verdict = format!("def {name} v{lv} ({desc:?}) reverted: superseded by v{v} before its soak finished");
            self.rec("def_reverted", json!({"name": name, "version": lv, "restored": restored, "error": "superseded", "verdict": verdict}));
        }
        if rec.kind != "note" {
            let code = self.live_code();
            if let Err(e) = self.vm.define(name, v, &rec.source, &code) {
                self.rec("def_rejected", json!({"name": name, "version": v, "error": e}));
                trace(&format!("[gate] {name} v{v} rejected: {}", first_line(&e, 120)));
                return Err(format!("rejected: compile in the live VM failed:\n{e}\nThe live version is unchanged."));
            }
        }
        self.rec("def_trial", json!({"name": name, "version": v, "kind": rec.kind, "source": rec.source,
            "description": rec.description, "tests": rec.tests, "input_schema": rec.input_schema, "origin": origin}));
        let soak = self.cfg.soak;
        Ok(match rec.kind.as_str() {
            "note" => format!("trial v{v}: note `{name}` is live; kept after {soak} tasks that do not fail."),
            _ => format!("trial v{v}: compiled in a scratch VM and passed {} test(s); `{name}` is live now and is kept after {soak} clean uses (any error reverts it).",
                rec.tests.as_array().map_or(0, Vec::len)),
        })
    }

    /// Records one use of a live definition and applies the soak rules.
    /// Returns a note for the model when the use reverted the trial.
    fn record_use(&mut self, name: &str, version: u32, result: &Result<String, String>, input: &Value) -> Option<String> {
        let error = result.as_ref().err().cloned();
        self.rec("def_use", json!({"name": name, "version": version, "ok": error.is_none(), "error": error}));
        let def = self.state.defs.get(name)?;
        let live = def.live_version().filter(|l| l.version == version && l.status == "trial")?;
        let (desc, is_code) = (live.description.clone(), live.kind != "note");
        if let Some(err) = error {
            let restored = def.last_kept();
            let verdict = format!("def {name} v{version} ({desc:?}) reverted: {} on input {}", first_line(&err, 200), compact(input, 200));
            self.rec("def_reverted", json!({"name": name, "version": version, "restored": restored, "error": err, "verdict": verdict}));
            let now = restored.map_or("definition removed".into(), |r| format!("live: v{r}"));
            trace(&format!("[gate] {name} v{version} reverted: {} ({now})", first_line(&err, 100)));
            if is_code {
                self.sync_slot(name, restored);
            }
            return Some(format!("trial v{version} of `{name}` was reverted by this error ({now}); the verdict is recorded."));
        } else if def.soak_uses >= self.cfg.soak {
            let verdict = format!("def {name} v{version} ({desc:?}) kept after {} clean uses", def.soak_uses);
            self.rec("def_kept", json!({"name": name, "version": version, "verdict": verdict}));
            trace(&format!("[gate] {name} v{version} kept"));
        }
        None
    }

    /// Late binding: resolves `name` in the registry on every call; inside
    /// the VM every call goes through the slot named `name`.
    pub fn call_def(&mut self, name: &str, input: &Value, check: fn(&str) -> Result<(), String>) -> Result<String, String> {
        let Some(live) = self.state.live(name).cloned() else {
            return Err(format!("no live definition `{name}`"));
        };
        if live.kind == "note" {
            return Err(format!("`{name}` is a note, not code"));
        }
        self.sync_slot(name, Some(live.version));
        let code = self.live_code();
        // Snapshot the whole Forth image (linear memory) before the call.
        let snap = self.vm.snapshot();
        let input_text = input.to_string();
        let out = self.vm.call(name, input_text.as_bytes(), &code);
        for (n, v) in &out.loads {
            trace(&format!("[forth] lazy-load {n} v{v}"));
            self.lazy_loads.push((n.clone(), *v));
        }
        let result = out.result.and_then(|s| check(&s).map(|()| s));
        if let Err(e) = &result
            && (out.trapped || live.status == "trial")
        {
            let why = first_line(e, 60);
            match self.vm.restore(&snap) {
                Ok(()) => trace(&format!("[forth] snapshot restored ({} KB) after: {why}", snap.len() >> 10)),
                Err(re) => {
                    trace(&format!("[forth] snapshot restore failed ({re}); starting a fresh VM"));
                    match ForthVm::new(self.cfg.limits.clone(), self.cfg.work_dir.clone(), protected_paths(&self.cfg)) {
                        Ok(vm) => {
                            self.vm = vm;
                            self.load_seeds();
                        }
                        Err(e2) => trace(&format!("[forth] fresh VM failed: {e2}")),
                    }
                }
            }
        }
        for l in out.lessons {
            let _ = self.remember(&l, "forth");
        }
        let note = self.record_use(name, live.version, &result, input);
        match (result, note) {
            (Err(e), Some(n)) => Err(format!("{e}\n{n}")),
            (r, _) => r,
        }
    }

    // ------------------------------------------------------------ prompting

    fn select_lessons(&mut self, task: &str) -> Vec<i64> {
        let lessons: Vec<Value> =
            self.state.active_lessons().map(|l| json!({"id": l.id, "text": l.text, "score": l.score})).collect();
        let check: fn(&str) -> Result<(), String> = |out| check_output("select_lessons", out);
        let ids = match self.call_def("select_lessons", &json!({"task": task, "lessons": lessons}), check) {
            Ok(out) => serde_json::from_str::<Vec<i64>>(&out).unwrap_or_default(),
            Err(e) => {
                trace(&format!("[hook] select_lessons failed, using default: {}", first_line(&e, 100)));
                default_select(task, &self.state)
            }
        };
        let mut seen = std::collections::HashSet::new();
        ids.into_iter()
            .filter(|id| self.state.active_lessons().any(|l| l.id == *id) && seen.insert(*id))
            .collect()
    }

    /// System blocks: the immutable base prompt, then the supplemental prompt
    /// built by the `compose_system` hook plus the verdicts (which bypass
    /// `select_lessons` and cannot be dropped by a hook).
    pub fn system_prompt(&mut self, selected: &[i64]) -> Vec<String> {
        let notes: Vec<String> = self
            .state
            .defs
            .values()
            .filter_map(|d| d.live_version())
            .filter(|v| v.kind == "note")
            .map(|v| v.source.clone())
            .collect();
        let lessons: Vec<String> = selected
            .iter()
            .filter_map(|id| self.state.lessons.iter().find(|l| l.id == *id))
            .map(|l| l.text.clone())
            .collect();
        let mut supplemental = match self.call_def("compose_system", &json!({"notes": notes, "lessons": lessons}), |_| Ok(())) {
            Ok(s) => s,
            Err(e) => {
                trace(&format!("[hook] compose_system failed, using default: {}", first_line(&e, 100)));
                default_compose(&notes, &lessons)
            }
        };
        let verdicts: Vec<&str> = self.state.verdicts.iter().rev().take(20).map(|v| v.text.as_str()).collect();
        if !verdicts.is_empty() {
            supplemental.push_str("\n\n## Verdicts on past userspace changes\n");
            for v in verdicts.iter().rev() {
                supplemental.push_str(&format!("- {v}\n"));
            }
        }
        vec![self.base_prompt.clone(), FORTH_REFERENCE.trim().to_string(), supplemental.trim().to_string()]
    }

    pub fn tool_list(&self) -> Vec<Value> {
        let mut list = tools::schemas();
        for def in self.state.defs.values() {
            if let Some(v) = def.live_version().filter(|v| v.kind == "tool") {
                list.push(json!({"name": def.name, "description": v.description, "input_schema": v.input_schema}));
            }
        }
        list
    }

    // ---------------------------------------------------------------- tools

    fn exec_tool(&mut self, name: &str, input: &Value) -> Result<String, String> {
        let wd = self.cfg.work_dir.clone();
        let int = |k: &str| input.get(k).and_then(Value::as_u64).map(|v| v as u32);
        let text = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        match name {
            "bash" => tools::bash(&wd, input),
            "read_file" => tools::read_file(&wd, input),
            "write_file" => {
                tools::write_file(&wd, input, &protected_paths(&self.cfg))
            }
            "list_defs" => Ok(self.defs_table()),
            "show_def" => self.show_def(&text("name"), int("version")),
            "forth_eval" => self.forth_eval(input),
            "propose_def" => self.propose_def(input),
            "rollback_def" => match int("version") {
                Some(v) => self.rollback_def(&text("name"), v),
                None => Err("`version` (integer) is required".into()),
            },
            "remember" => self.remember(&text("text"), "remember"),
            "recall" => Ok(self.recall(&text("query"))),
            _ if self.state.live(name).is_some_and(|v| v.kind == "tool") => {
                self.call_def(name, input, |_| Ok(())).map(|s| tools::truncate(&s, 20 * 1024))
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
            let mut results = Vec::new();
            for b in resp.content.iter().filter(|b| b["type"] == "tool_use") {
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

        // Notes soak over tasks: a task that does not succeed counts as an error.
        let trial_notes: Vec<(String, u32)> = self
            .state
            .defs
            .values()
            .filter_map(|d| d.live_version().filter(|v| v.kind == "note" && v.status == "trial").map(|v| (d.name.clone(), v.version)))
            .collect();
        for (name, v) in trial_notes {
            let r = if success { Ok(String::new()) } else { Err(format!("task {task_id} failed")) };
            self.record_use(&name, v, &r, &json!({"task": task}));
        }

        // Score the lessons that were in the prompt.
        let delta = if success { 1 } else { -1 };
        for id in &selected {
            let Some(l) = self.state.lessons.iter().find(|l| l.id == *id) else { continue };
            let score = l.score + delta;
            self.rec("lesson_scored", json!({"id": id, "delta": delta, "score": score, "task_id": task_id}));
            if score <= -3 {
                self.rec("lesson_retired", json!({"id": id}));
                trace(&format!("[learn] lesson #{id} retired (score {score})"));
            }
        }
        let summary = first_line(&answer, 200);
        self.rec("task_finished", json!({"task_id": task_id, "success": success, "error": kernel_error, "summary": summary}));
        trace(&format!("[task] {task_id} {}", if success { "succeeded" } else { "failed" }));
        transcript.push(format!("OUTCOME: {}", if success { "success" } else { "failure" }));
        if self.cfg.reflect {
            self.reflect(&transcript.join("\n"));
        }
        TaskOutcome { success, answer, error: kernel_error }
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

    pub fn state_dir(&self) -> &Path {
        &self.cfg.state_dir
    }
}

/// Paths no kernel tool or host word may write: the state dir, the kernel
/// (host crate) and the base prompt.
fn protected_paths(cfg: &Config) -> Vec<PathBuf> {
    let wd = &cfg.work_dir;
    vec![tools::resolve(wd, &cfg.state_dir.to_string_lossy()), tools::resolve(wd, KERNEL_SRC), tools::resolve(wd, BASE_PROMPT_PATH)]
}

/// Shape checks on hook output ("bad return type" is an error).
fn check_output(name: &str, out: &str) -> Result<(), String> {
    if name == "select_lessons" {
        serde_json::from_str::<Vec<i64>>(out.trim())
            .map(|_| ())
            .map_err(|e| format!("contract: select_lessons must return a JSON array of lesson ids like [3,1]; got {:?} ({e})", first_line(out, 200)))?;
    }
    Ok(())
}

fn tool_trace(name: &str, input: &Value, result: &Result<String, String>) -> String {
    let arg = match name {
        "bash" => first_line(input["command"].as_str().unwrap_or(""), 60),
        "propose_def" | "show_def" | "rollback_def" => {
            let v = input.get("version").map(|v| format!(" {v}")).unwrap_or_default();
            format!("{}{v}", input["name"].as_str().unwrap_or("?"))
        }
        "read_file" | "write_file" => input["path"].as_str().unwrap_or("").to_string(),
        "forth_eval" => first_line(input["source"].as_str().unwrap_or(""), 60),
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

pub fn default_compose(notes: &[String], lessons: &[String]) -> String {
    let mut out = String::new();
    if !notes.is_empty() {
        out.push_str("## Notes\n");
        for n in notes {
            out.push_str(n.trim());
            out.push_str("\n\n");
        }
    }
    if !lessons.is_empty() {
        out.push_str("## Lessons\n");
        for l in lessons {
            out.push_str(&format!("- {}\n", l.trim()));
        }
    }
    out.trim_end().to_string()
}

fn default_select(task: &str, state: &State) -> Vec<i64> {
    let tw = words(task, 4);
    let mut all: Vec<_> = state.active_lessons().map(|l| (l.id, l.score, !words(&l.text, 4).is_disjoint(&tw))).collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let hits: Vec<i64> = all.iter().filter(|l| l.2).take(8).map(|l| l.0).collect();
    if hits.is_empty() { all.iter().take(3).map(|l| l.0).collect() } else { hits }
}
