//! The kernel: agent loop, prompt assembly, the gate, knowledge, journal
//! replay and the Forth userspace. This whole crate is the kernel image; the
//! agent can change it with `propose_kernel` (the hypervisor builds, boots
//! and self-tests the candidate), but never directly.

use crate::forth::{ForthIo, ForthVm, Live, Snapshot};
use crate::state::{DefVersion, State};
use crate::sys::Sys;
use crate::tools;
use crate::util::{clip, compact, first_line, normalize, str_field, truncate, words};
use serde_json::{Value, json};
use std::rc::Rc;

/// Kernel-owned system block after the base prompt: the Forth dialect.
pub const FORTH_REFERENCE: &str = include_str!("forth_reference.md");
/// Kernel-owned system block: how this harness can change itself.
pub const KERNEL_REFERENCE: &str = include_str!("kernel_reference.md");

pub const REFINE_PROMPT: &str = "Refine yourself. Review the recent task outcomes below, the verdicts in your system prompt, \
and your current userspace (use list_defs, show_def and recall). Then improve userspace through propose_def or knowledge \
through remember. Make at most 3 changes, prefer small ones, and give every tool or hook real tests. \
Finish with a short summary of what you changed and why.";

/// Seed userspace, installed as kept v1 when the journal is empty. The live
/// versions of these names are compiled eagerly at startup; everything else
/// loads lazily.
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
        source: include_str!("../seed/compose_system.fs"),
        description: "Joins notes under a Notes heading and lessons under a Lessons heading.",
        tests: r###"[{"input": {"notes": ["Be brief."], "lessons": ["Run tests first."]}, "expect": "## Lessons\n- Run tests first."}]"###,
        input_schema: "null",
    },
    Seed {
        name: "select_lessons",
        kind: "hook",
        source: include_str!("../seed/select_lessons.fs"),
        description: "Top 8 lessons by score sharing a 4+ letter word with the task, else top 3 by score.",
        tests: r#"[{"input": {"task": "fix the parser bug", "lessons": [{"id": 1, "text": "the parser needs tests", "score": 0}, {"id": 2, "text": "unrelated", "score": 5}]}, "expect": "[1]"}]"#,
        input_schema: "null",
    },
    Seed {
        name: "word_count",
        kind: "tool",
        source: include_str!("../seed/word_count.fs"),
        description: "Counts whitespace-separated words in `text`.",
        tests: r#"[{"input": {"text": "one two three"}, "expect": "3"}]"#,
        input_schema: r#"{"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}"#,
    },
];

/// Run settings, from the hypervisor's `config`.
pub struct Config {
    pub work_dir: String,
    pub soak: u32,
    pub reflect: bool,
    pub max_turns: usize,
    pub base_prompt: String,
    /// Print assistant text to stdout as it arrives (repl).
    pub echo_text: bool,
}

impl Config {
    fn from_json(v: &Value) -> Config {
        let int = |k: &str, d: u64| v.get(k).and_then(Value::as_u64).unwrap_or(d);
        Config {
            work_dir: str_field(v, "work-dir"),
            soak: int("soak", 3).max(1) as u32,
            reflect: v.get("reflect").and_then(Value::as_bool).unwrap_or(true),
            max_turns: int("max-turns", 40) as usize,
            base_prompt: str_field(v, "base-prompt"),
            echo_text: v.get("echo-text").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

pub struct TaskOutcome {
    pub success: bool,
    pub answer: String,
    pub error: Option<String>,
}

/// File access for the Forth host words: inside the work dir only. The
/// hypervisor refuses protected paths again on every write.
struct KernelIo {
    sys: Rc<dyn Sys>,
    work_dir: String,
}

impl KernelIo {
    fn check(&self, raw: &str) -> Result<String, String> {
        if raw.split(['/', '\\']).any(|c| c == "..") {
            return Err(format!("refused `{raw}`: `..` is not allowed; paths stay inside the working directory"));
        }
        let path = normalize(&self.work_dir, raw);
        let wd = self.work_dir.trim_end_matches('/');
        if path != wd && !path.starts_with(&format!("{wd}/")) {
            return Err(format!("refused `{raw}`: outside the working directory {wd}"));
        }
        Ok(path)
    }
}

impl ForthIo for KernelIo {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let p = self.check(path)?;
        self.sys.read_file(&p)
    }
    fn write(&mut self, path: &str, content: &[u8]) -> Result<(), String> {
        let p = self.check(path)?;
        self.sys.write_file(&p, &String::from_utf8_lossy(content)).map(|_| ())
    }
}

pub struct Kernel {
    pub cfg: Config,
    pub state: State,
    sys: Rc<dyn Sys>,
    /// The live Forth image. Validation never touches it.
    vm: ForthVm,
    /// Definitions compiled lazily so far in this process (name, version).
    pub lazy_loads: Vec<(String, u32)>,
    /// Journal events applied so far (see `sync`).
    seen: usize,
}

impl Kernel {
    /// Replays the journal (installing the seed userspace if it is empty).
    pub fn open(sys: Rc<dyn Sys>) -> Result<Kernel, String> {
        let cfg = Config::from_json(&sys.config());
        let vm = ForthVm::new(Box::new(KernelIo { sys: sys.clone(), work_dir: cfg.work_dir.clone() }));
        let mut k = Kernel { cfg, state: State::default(), sys, vm, lazy_loads: Vec::new(), seen: 0 };
        let events = k.sys.journal_read();
        for ev in &events {
            k.state.apply(ev);
        }
        k.seen = events.len();
        if events.is_empty() {
            for s in &SEEDS {
                k.record(
                    "def_trial",
                    json!({
                        "name": s.name, "version": 1, "kind": s.kind, "source": s.source,
                        "description": s.description, "origin": "seed",
                        "tests": serde_json::from_str::<Value>(s.tests).expect("seed tests"),
                        "input_schema": serde_json::from_str::<Value>(s.input_schema).expect("seed schema"),
                    }),
                )?;
                k.record("def_kept", json!({"name": s.name, "version": 1, "seed": true, "verdict": ""}))?;
            }
        }
        k.load_seeds();
        Ok(k)
    }

    fn trace(&self, line: &str) {
        self.sys.log(line);
    }

    fn new_vm(&self) -> ForthVm {
        ForthVm::new(Box::new(KernelIo { sys: self.sys.clone(), work_dir: self.cfg.work_dir.clone() }))
    }

    /// Compiles the live versions of the seed-named definitions into the
    /// live VM. Every other definition is compiled on its first call.
    fn load_seeds(&mut self) {
        let code = live_code(&self.state);
        for name in SEEDS.map(|s| s.name) {
            if let Some((v, src)) = code.get(name)
                && let Err(e) = self.vm.define(name, *v, src, &code)
            {
                self.sys.log(&format!("[forth] could not load {name} v{v}: {}", first_line(&e, 160)));
            }
        }
    }

    /// Makes the VM's public slot for `name` match the registry.
    fn sync_slot(&mut self, name: &str, version: Option<u32>) {
        match version {
            Some(v) if self.vm.vm.bind(name, v) => {}
            _ => self.vm.vm.unbind(name),
        }
    }

    /// Version currently bound in the live VM (0 = not compiled yet).
    pub fn vm_bound(&self, name: &str) -> u32 {
        self.vm.vm.bound(name)
    }

    fn record(&mut self, kind: &str, fields: Value) -> Result<Value, String> {
        let mut ev = json!({"type": kind});
        if let Value::Object(m) = fields {
            ev.as_object_mut().expect("object").extend(m);
        }
        let stored = self.sys.journal_append(&ev)?;
        self.state.apply(&stored);
        self.seen += 1;
        Ok(stored)
    }

    /// Applies events the hypervisor appended since we last looked (image
    /// events, and `task_finished` for a task an instance could not finish).
    pub fn sync(&mut self) {
        let events = self.sys.journal_read();
        for ev in events.iter().skip(self.seen) {
            self.state.apply(ev);
        }
        self.seen = self.seen.max(events.len());
    }

    /// `record` for events whose loss must not stop the agent.
    fn record_or_trace(&mut self, kind: &str, fields: Value) {
        if let Err(e) = self.record(kind, fields) {
            self.trace(&format!("[journal] write failed: {e}"));
        }
    }

    // ----------------------------------------------------------------- gate

    /// Shape checks, then compiles `source` in a scratch VM and runs its tests there.
    fn validate(&self, name: &str, kind: &str, version: u32, source: &str, tests: &Value, schema: &Value) -> Result<(), String> {
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
            return Ok(());
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
        // Scratch: a brand-new VM. The live VM is not touched.
        let code = live_code(&self.state);
        let mut scratch = self.new_vm();
        scratch.define(name, version, source, &code).map_err(|e| format!("compile failed:\n{e}"))?;
        for (i, t) in tests.iter().enumerate() {
            let expect = t["expect"].as_str().unwrap_or_default();
            let input = t["input"].to_string();
            let out = scratch.call(name, input.as_bytes(), &code);
            let result = out.result.and_then(|o| check_output(name, &o).map(|()| o));
            match result {
                Ok(out) if out.contains(expect) => {}
                Ok(out) => {
                    return Err(format!(
                        "test {} failed: input {} expected substring {expect:?}, got {:?}",
                        i + 1,
                        compact(&t["input"], 300),
                        truncate(&out, 500)
                    ));
                }
                Err(e) => return Err(format!("test {} failed:\n{e}", i + 1)),
            }
        }
        self.trace(&format!("[gate] scratch vm: {name} v{version} compiled, {} test(s) passed", tests.len()));
        Ok(())
    }

    /// Scratchpad: compiles and runs a snippet in a throwaway VM.
    fn forth_eval(&self, input: &Value) -> Result<String, String> {
        let source = str_field(input, "source");
        if source.trim().is_empty() {
            return Err("source is empty".into());
        }
        let arg = input.get("input").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        let code = live_code(&self.state);
        let mut vm = self.new_vm();
        vm.define("scratchpad", 1, &source, &code).map_err(|e| format!("compile failed:\n{e}"))?;
        let out = vm.call("scratchpad", arg.to_string().as_bytes(), &code);
        out.result.map(|o| format!("output: {}", truncate(&o, 20 * 1024)))
    }

    /// The gate. Returns the verdict text for the model, `Err` if rejected.
    pub fn propose_def(&mut self, input: &Value) -> Result<String, String> {
        let (name, kind) = (str_field(input, "name"), str_field(input, "kind"));
        let (source, description) = (str_field(input, "source"), str_field(input, "description"));
        let tests = input.get("tests").cloned().unwrap_or(json!([]));
        let schema = if kind == "tool" { input.get("input_schema").cloned().unwrap_or(Value::Null) } else { Value::Null };
        self.record_or_trace(
            "def_proposed",
            json!({"name": name, "kind": kind, "source": source,
            "description": description, "tests": tests, "input_schema": schema}),
        );
        let version = self.state.defs.get(&name).map_or(1, |d| d.next_version());
        let candidate = DefVersion { version, kind, source, description, tests, input_schema: schema, status: "trial".into() };
        self.install_trial(&name, candidate, "propose")
    }

    pub fn rollback_def(&mut self, name: &str, version: u32) -> Result<String, String> {
        let def = self.state.defs.get(name).ok_or(format!("no definition named `{name}`"))?;
        let old = def.get(version).ok_or(format!("`{name}` has no version {version}"))?.clone();
        let candidate = DefVersion { version: def.next_version(), status: "trial".into(), ..old };
        self.install_trial(name, candidate, &format!("rollback to v{version}"))
    }

    fn install_trial(&mut self, name: &str, candidate: DefVersion, origin: &str) -> Result<String, String> {
        let v = candidate.version;
        if let Err(e) = self.validate(name, &candidate.kind, v, &candidate.source, &candidate.tests, &candidate.input_schema) {
            return self.reject(name, v, &e, &e);
        }
        // A trial that is still soaking is superseded (reverted) first.
        if let Some(def) = self.state.defs.get(name)
            && let Some(live) = def.live_version()
            && live.status == "trial"
        {
            let (lv, desc, restored) = (live.version, live.description.clone(), def.last_kept());
            let verdict = format!("def {name} v{lv} ({desc:?}) reverted: superseded by v{v} before its soak finished");
            self.record_or_trace(
                "def_reverted",
                json!({"name": name, "version": lv, "restored": restored, "error": "superseded", "verdict": verdict}),
            );
        }
        if candidate.kind != "note" {
            let code = live_code(&self.state);
            if let Err(e) = self.vm.define(name, v, &candidate.source, &code) {
                return self.reject(name, v, &e, &format!("compile in the live VM failed:\n{e}"));
            }
        }
        self.record_or_trace(
            "def_trial",
            json!({"name": name, "version": v, "kind": candidate.kind, "source": candidate.source,
            "description": candidate.description, "tests": candidate.tests, "input_schema": candidate.input_schema, "origin": origin}),
        );
        let soak = self.cfg.soak;
        Ok(match candidate.kind.as_str() {
            "note" => format!("trial v{v}: note `{name}` is live; kept after {soak} tasks that do not fail."),
            _ => format!(
                "trial v{v}: compiled in a scratch VM and passed {} test(s); `{name}` is live now and is kept after {soak} clean uses (any error reverts it).",
                candidate.tests.as_array().map_or(0, Vec::len)
            ),
        })
    }

    fn reject(&mut self, name: &str, version: u32, error: &str, shown: &str) -> Result<String, String> {
        self.record_or_trace("def_rejected", json!({"name": name, "version": version, "error": error}));
        self.trace(&format!("[gate] {name} v{version} rejected: {}", first_line(error, 120)));
        Err(format!("rejected: {shown}\nThe live version is unchanged."))
    }

    /// Records one use of a live definition and applies the soak rules.
    fn record_use(&mut self, name: &str, version: u32, result: &Result<String, String>, input: &Value) -> Option<String> {
        let error = result.as_ref().err().cloned();
        self.record_or_trace("def_use", json!({"name": name, "version": version, "ok": error.is_none(), "error": error}));
        let def = self.state.defs.get(name)?;
        let live = def.live_version().filter(|l| l.version == version && l.status == "trial")?;
        let (desc, is_code) = (live.description.clone(), live.kind != "note");
        if let Some(err) = error {
            let restored = def.last_kept();
            let verdict = format!("def {name} v{version} ({desc:?}) reverted: {} on input {}", first_line(&err, 200), compact(input, 200));
            self.record_or_trace(
                "def_reverted",
                json!({"name": name, "version": version, "restored": restored, "error": err, "verdict": verdict}),
            );
            let now = restored.map_or("definition removed".into(), |r| format!("live: v{r}"));
            self.trace(&format!("[gate] {name} v{version} reverted: {} ({now})", first_line(&err, 100)));
            if is_code {
                self.sync_slot(name, restored);
            }
            return Some(format!("trial v{version} of `{name}` was reverted by this error ({now}); the verdict is recorded."));
        } else if def.soak_uses >= self.cfg.soak {
            let verdict = format!("def {name} v{version} ({desc:?}) kept after {} clean uses", def.soak_uses);
            self.record_or_trace("def_kept", json!({"name": name, "version": version, "verdict": verdict}));
            self.trace(&format!("[gate] {name} v{version} kept"));
        }
        None
    }

    /// Late binding: resolves `name` in the registry on every call; inside
    /// the VM every call goes through the slot named `name`.
    pub fn call_def(&mut self, name: &str, input: &Value) -> Result<String, String> {
        let Some((version, is_note, is_trial)) = self.state.live(name).map(|v| (v.version, v.kind == "note", v.status == "trial")) else {
            return Err(format!("no live definition `{name}`"));
        };
        if is_note {
            return Err(format!("`{name}` is a note, not code"));
        }
        self.sync_slot(name, Some(version));
        let code = live_code(&self.state);
        let snap = self.vm.vm.snapshot();
        let input_text = input.to_string();
        let out = self.vm.call(name, input_text.as_bytes(), &code);
        for (n, v) in &out.loads {
            self.trace(&format!("[forth] lazy-load {n} v{v}"));
            self.lazy_loads.push((n.clone(), *v));
        }
        let result = out.result.and_then(|s| check_output(name, &s).map(|()| s));
        // A failing trial must not keep anything it changed (it is reverted below).
        if result.is_err() && is_trial {
            self.restore_vm(&snap, &first_line(result.as_ref().err().map_or("", |e| e), 60));
        }
        for l in out.lessons {
            let _ = self.remember(&l, "forth");
        }
        let note = self.record_use(name, version, &result, input);
        match (result, note) {
            (Err(e), Some(n)) => Err(format!("{e}\n{n}")),
            (r, _) => r,
        }
    }

    fn restore_vm(&mut self, snap: &Snapshot, why: &str) {
        self.vm.vm.restore(snap);
        self.trace(&format!("[forth] snapshot restored ({} KB) after: {why}", snap.len() >> 10));
    }

    // ------------------------------------------------------------ prompting

    fn select_lessons(&mut self, task: &str) -> Vec<i64> {
        let lessons: Vec<Value> = self.state.active_lessons().map(|l| json!({"id": l.id, "text": l.text, "score": l.score})).collect();
        let ids = match self.call_def("select_lessons", &json!({"task": task, "lessons": lessons})) {
            Ok(out) => serde_json::from_str::<Vec<i64>>(&out).unwrap_or_default(),
            Err(e) => {
                self.trace(&format!("[hook] select_lessons failed, using default: {}", first_line(&e, 100)));
                default_select(task, &self.state)
            }
        };
        let mut seen = std::collections::HashSet::new();
        ids.into_iter().filter(|id| self.state.active_lessons().any(|l| l.id == *id) && seen.insert(*id)).collect()
    }

    /// System blocks: the immutable base prompt, the kernel's references,
    /// then the supplemental prompt built by the `compose_system` hook plus
    /// the verdicts (which bypass `select_lessons`).
    pub fn system_prompt(&mut self, selected: &[i64]) -> Vec<String> {
        let notes: Vec<String> = self.state.live_defs().filter(|(_, v)| v.kind == "note").map(|(_, v)| v.source.clone()).collect();
        let lessons: Vec<String> =
            selected.iter().filter_map(|id| self.state.lessons.iter().find(|l| l.id == *id)).map(|l| l.text.clone()).collect();
        let mut supplemental = match self.call_def("compose_system", &json!({"notes": notes, "lessons": lessons})) {
            Ok(s) => s,
            Err(e) => {
                self.trace(&format!("[hook] compose_system failed, using default: {}", first_line(&e, 100)));
                default_compose(&notes, &lessons)
            }
        };
        let verdicts: Vec<&str> = self.state.verdicts.iter().rev().take(20).map(|v| v.text.as_str()).collect();
        if !verdicts.is_empty() {
            supplemental.push_str("\n\n## Verdicts on past userspace and kernel changes\n");
            for v in verdicts.iter().rev() {
                supplemental.push_str(&format!("- {v}\n"));
            }
        }
        vec![
            self.cfg.base_prompt.clone(),
            FORTH_REFERENCE.trim().to_string(),
            KERNEL_REFERENCE.trim().to_string(),
            supplemental.trim().to_string(),
        ]
    }

    pub fn tool_list(&self) -> Vec<Value> {
        let mut list = tools::schemas();
        for (def, v) in self.state.live_defs().filter(|(_, v)| v.kind == "tool") {
            list.push(json!({"name": def.name, "description": v.description, "input_schema": v.input_schema}));
        }
        list
    }

    // ---------------------------------------------------------------- tools

    fn exec_tool(&mut self, name: &str, input: &Value) -> Result<String, String> {
        let int = |k: &str| input.get(k).and_then(Value::as_u64).map(|v| v as u32);
        let text = |k: &str| str_field(input, k);
        let sys = self.sys.clone();
        match name {
            "bash" => tools::bash(&*sys, input),
            "read_file" => tools::read_file(&*sys, &self.cfg.work_dir, input),
            "write_file" => tools::write_file(&*sys, &self.cfg.work_dir, input),
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
            "propose_kernel" => {
                let verdict = sys.propose_image(input);
                self.sync();
                verdict
            }
            "list_images" => Ok(sys.images()),
            _ if self.state.live(name).is_some_and(|v| v.kind == "tool") => self.call_def(name, input).map(|s| truncate(&s, 20 * 1024)),
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
        let mut rec = v.to_json();
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

    /// Non-task commands (CLI `defs`/`knowledge`, repl slash commands, and
    /// `/state`, `/call`, `/propose`, `/remember` for tests and power users).
    pub fn command(&mut self, line: &str) -> Result<String, String> {
        let line = line.trim();
        let (cmd, rest) = line.split_once(' ').map_or((line, ""), |(c, r)| (c, r.trim()));
        let version = |s: &str| s.trim_start_matches('v').parse::<u32>().map_err(|_| format!("bad version `{s}`"));
        match cmd.trim_start_matches('/') {
            "defs" => Ok(self.defs_table()),
            "knowledge" => Ok(self.knowledge_text()),
            "state" => Ok(self.state.to_json().to_string()),
            "show" => match rest.split_whitespace().collect::<Vec<_>>().as_slice() {
                [name] => self.show_def(name, None),
                [name, v] => self.show_def(name, Some(version(v)?)),
                _ => Err("usage: /show NAME [VERSION]".into()),
            },
            "rollback" => match rest.split_whitespace().collect::<Vec<_>>().as_slice() {
                [name, v] => self.rollback_def(name, version(v)?),
                _ => Err("usage: /rollback NAME VERSION".into()),
            },
            "call" => {
                let (name, json_text) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let input: Value = serde_json::from_str(json_text).map_err(|e| format!("bad JSON input: {e}"))?;
                self.call_def(name, &input)
            }
            "propose" => {
                let input: Value = serde_json::from_str(rest).map_err(|e| format!("bad JSON: {e}"))?;
                self.propose_def(&input)
            }
            "remember" => self.remember(rest, "remember"),
            "bound" => Ok(self.vm_bound(rest).to_string()),
            "lazy" => Ok(json!(self.lazy_loads).to_string()),
            "stats" => Ok(json!(self.vm.vm.stats()).to_string()),
            other => Err(format!("unknown command `{other}`")),
        }
    }

    // ----------------------------------------------------------- agent loop

    pub fn run_task(&mut self, task: &str) -> TaskOutcome {
        let task_id = self.state.tasks.len() as u64 + 1;
        self.record_or_trace("task_started", json!({"task_id": task_id, "task": task}));
        let selected = self.select_lessons(task);
        let mut run = TaskRun {
            task_id,
            task: task.to_string(),
            selected,
            messages: vec![json!({"role": "user", "content": task})],
            transcript: vec![format!("TASK: {task}")],
            tool_errors: Vec::new(),
            turn: 0,
        };
        let turns = self.run_turns(&mut run);
        self.finish_task(run, turns)
    }

    /// Continues a task after the previous instance trapped: every tool call
    /// of the last turn gets the trap report as its (error) result.
    pub fn resume_task(&mut self, checkpoint: &str, report: &str) -> TaskOutcome {
        let Some(mut run) = TaskRun::from_json(checkpoint) else {
            return TaskOutcome { success: false, answer: String::new(), error: Some("unreadable checkpoint".into()) };
        };
        let pending: Vec<Value> = run
            .messages
            .last()
            .and_then(|m| m["content"].as_array())
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                json!({"type": "tool_result", "tool_use_id": b["id"], "is_error": true, "content": format!(
                    "error: the kernel image trapped during this turn's tool calls; the hypervisor restored it and replayed the journal. \
                     Work from tool calls in this turn may be lost.\n{report}")})
            })
            .collect();
        self.trace(&format!("[kernel] resuming task {} after a trap ({} tool call(s) failed)", run.task_id, pending.len()));
        run.transcript.push(format!("KERNEL TRAP: {}", first_line(report, 200)));
        run.tool_errors.extend(pending.iter().map(|_| true));
        if !pending.is_empty() {
            run.messages.push(json!({"role": "user", "content": pending}));
        }
        run.turn += 1;
        let turns = self.run_turns(&mut run);
        self.finish_task(run, turns)
    }

    fn finish_task(&mut self, mut run: TaskRun, turns: Turns) -> TaskOutcome {
        // Success: a clean end_turn, and neither of the last two tool calls failed.
        let tail_ok = run.tool_errors.iter().rev().take(2).all(|e| !e);
        let success = turns.kernel_error.is_none() && turns.stop == "end_turn" && tail_ok;
        self.sys.checkpoint(&json!({}));
        self.soak_trial_notes(&run.task, run.task_id, success);
        self.score_lessons(&run.selected, run.task_id, success);
        let summary = first_line(&turns.answer, 200);
        self.record_or_trace(
            "task_finished",
            json!({"task_id": run.task_id, "success": success, "error": turns.kernel_error, "summary": summary}),
        );
        self.trace(&format!("[task] {} {}", run.task_id, if success { "succeeded" } else { "failed" }));
        run.transcript.push(format!("OUTCOME: {}", if success { "success" } else { "failure" }));
        if self.cfg.reflect {
            self.reflect(&run.transcript.join("\n"));
        }
        TaskOutcome { success, answer: turns.answer, error: turns.kernel_error }
    }

    /// The model/tool loop: one model request per turn, then the tool calls
    /// it asked for, until the model stops or something fails.
    fn run_turns(&mut self, run: &mut TaskRun) -> Turns {
        let mut turns = Turns { answer: String::new(), stop: String::new(), kernel_error: None };
        while run.turn < self.cfg.max_turns {
            // Recomposed every turn: each compose_system call is a hook use.
            let system = self.system_prompt(&run.selected);
            let body = json!({"system": system, "tools": self.tool_list(), "messages": run.messages});
            let resp = match self.sys.complete(&body) {
                Ok(r) => r,
                Err(e) => {
                    self.trace(&format!("[kernel] model request failed: {}", first_line(&e, 200)));
                    turns.kernel_error = Some(e);
                    break;
                }
            };
            let content = resp["content"].as_array().cloned().unwrap_or_default();
            let text = response_text(&content);
            if !text.is_empty() {
                if self.cfg.echo_text {
                    self.sys.print(&text);
                } else {
                    self.trace(&format!("[assistant] {}", first_line(&text, 160)));
                }
                run.transcript.push(format!("ASSISTANT: {}", truncate(&text, 400)));
                turns.answer = text;
            }
            turns.stop = str_field(&resp, "stop_reason");
            run.messages.push(json!({"role": "assistant", "content": content}));
            if turns.stop != "tool_use" {
                break;
            }
            // If a tool call traps the image, the hypervisor resumes from here.
            self.sys.checkpoint(&run.to_json());
            let results = self.run_tool_calls(&content, run);
            if results.is_empty() {
                turns.kernel_error = Some("stop_reason tool_use without tool_use blocks".into());
                break;
            }
            run.messages.push(json!({"role": "user", "content": results}));
            run.turn += 1;
            if run.turn == self.cfg.max_turns {
                turns.kernel_error = Some(format!("turn limit ({}) reached", self.cfg.max_turns));
            }
        }
        turns
    }

    /// Runs every tool_use block of one response and returns the tool_result blocks.
    fn run_tool_calls(&mut self, content: &[Value], run: &mut TaskRun) -> Vec<Value> {
        let mut results = Vec::new();
        for b in content.iter().filter(|b| b["type"] == "tool_use") {
            let name = b["name"].as_str().unwrap_or("");
            let input = b.get("input").cloned().unwrap_or(json!({}));
            let result = self.exec_tool(name, &input);
            let (out, is_err) = match &result {
                Ok(s) => (s.clone(), false),
                Err(e) => (format!("error: {e}"), true),
            };
            self.trace(&tool_trace(name, &input, &result));
            run.transcript.push(format!(
                "TOOL {name} {} -> {}{}",
                compact(&input, 150),
                if is_err { "ERROR " } else { "" },
                first_line(&out, 150)
            ));
            run.tool_errors.push(is_err);
            results.push(json!({"type": "tool_result", "tool_use_id": b["id"], "content": out, "is_error": is_err}));
        }
        results
    }

    /// Notes soak over tasks: a task that does not succeed counts as an error.
    fn soak_trial_notes(&mut self, task: &str, task_id: u64, success: bool) {
        let trial_notes: Vec<(String, u32)> = self
            .state
            .live_defs()
            .filter(|(_, v)| v.kind == "note" && v.status == "trial")
            .map(|(d, v)| (d.name.clone(), v.version))
            .collect();
        for (name, v) in trial_notes {
            let r = if success { Ok(String::new()) } else { Err(format!("task {task_id} failed")) };
            self.record_use(&name, v, &r, &json!({"task": task}));
        }
    }

    fn score_lessons(&mut self, selected: &[i64], task_id: u64, success: bool) {
        let delta = if success { 1 } else { -1 };
        for id in selected {
            let Some(l) = self.state.lessons.iter().find(|l| l.id == *id) else { continue };
            let score = l.score + delta;
            self.record_or_trace("lesson_scored", json!({"id": id, "delta": delta, "score": score, "task_id": task_id}));
            if score <= -3 {
                self.record_or_trace("lesson_retired", json!({"id": id}));
                self.trace(&format!("[learn] lesson #{id} retired (score {score})"));
            }
        }
    }

    fn reflect(&mut self, transcript: &str) {
        let prompt = format!(
            "Reflect on this finished task.\n\n{}\n\nReply with only a JSON array of 0 to 3 short, reusable lessons \
             (strings) that would help with future tasks. Skip task-specific trivia; reply [] if there is nothing durable.",
            truncate(transcript, 12_000)
        );
        let body = json!({"system": [self.cfg.base_prompt], "messages": [{"role": "user", "content": prompt}]});
        let resp = match self.sys.complete(&body) {
            Ok(r) => r,
            Err(e) => {
                self.trace(&format!("[reflect] failed: {}", first_line(&e, 160)));
                return;
            }
        };
        let text: String = resp["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
        let parsed = text.find('[').zip(text.rfind(']')).and_then(|(a, b)| serde_json::from_str::<Vec<String>>(&text[a..=b]).ok());
        let Some(lessons) = parsed else {
            self.trace("[reflect] no JSON array in reply");
            return;
        };
        let mut added = 0;
        for l in lessons.iter().take(3) {
            if matches!(self.remember(l, "reflection"), Ok(m) if m.starts_with("lesson")) {
                added += 1;
            }
        }
        self.trace(&format!("[reflect] +{added} lesson(s)"));
    }

    pub fn refine(&mut self) -> TaskOutcome {
        let recent: Vec<String> = self
            .state
            .tasks
            .iter()
            .rev()
            .take(10)
            .map(|t| {
                let outcome = match t.success {
                    Some(true) => "success",
                    Some(false) => "failure",
                    None => "unfinished",
                };
                format!("- task {} [{outcome}]: {} => {}", t.id, first_line(&t.task, 120), t.summary)
            })
            .collect();
        let recent = if recent.is_empty() { "(no tasks yet)".into() } else { recent.into_iter().rev().collect::<Vec<_>>().join("\n") };
        self.run_task(&format!("{REFINE_PROMPT}\n\nRecent task outcomes:\n{recent}"))
    }
}

/// One task in flight. Saved as the hypervisor checkpoint before each
/// turn's tool calls; the field names are part of the checkpoint format.
struct TaskRun {
    task_id: u64,
    task: String,
    selected: Vec<i64>,
    messages: Vec<Value>,
    transcript: Vec<String>,
    tool_errors: Vec<bool>,
    turn: usize,
}

impl TaskRun {
    fn to_json(&self) -> Value {
        json!({"task_id": self.task_id, "task": self.task, "selected": self.selected, "messages": self.messages,
            "transcript": self.transcript, "tool_errors": self.tool_errors, "turn": self.turn})
    }

    fn from_json(text: &str) -> Option<TaskRun> {
        let v: Value = serde_json::from_str(text).ok()?;
        Some(TaskRun {
            task_id: v["task_id"].as_u64()?,
            task: str_field(&v, "task"),
            selected: v["selected"].as_array()?.iter().filter_map(Value::as_i64).collect(),
            messages: v["messages"].as_array()?.clone(),
            transcript: v["transcript"].as_array()?.iter().filter_map(|t| t.as_str().map(String::from)).collect(),
            tool_errors: v["tool_errors"].as_array()?.iter().filter_map(Value::as_bool).collect(),
            turn: v["turn"].as_u64()? as usize,
        })
    }
}

/// name -> (live version, source) for every compiled kind.
fn live_code(state: &State) -> Live<'_> {
    state.live_defs().filter(|(_, v)| v.kind != "note").map(|(d, v)| (d.name.as_str(), (v.version, v.source.as_str()))).collect()
}

/// How the turn loop of one task ended.
struct Turns {
    answer: String,
    stop: String,
    kernel_error: Option<String>,
}

fn response_text(content: &[Value]) -> String {
    let text: Vec<&str> = content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    text.join("\n").trim().to_string()
}

/// Shape checks on hook output ("bad return type" is an error).
fn check_output(name: &str, out: &str) -> Result<(), String> {
    if name == "select_lessons" {
        serde_json::from_str::<Vec<i64>>(out.trim()).map(|_| ()).map_err(|e| {
            format!("contract: select_lessons must return a JSON array of lesson ids like [3,1]; got {:?} ({e})", first_line(out, 200))
        })?;
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
        "propose_kernel" => clip(input["description"].as_str().unwrap_or(""), 60),
        "read_file" | "write_file" => input["path"].as_str().unwrap_or("").to_string(),
        "forth_eval" => first_line(input["source"].as_str().unwrap_or(""), 60),
        "remember" | "recall" => first_line(input["text"].as_str().or(input["query"].as_str()).unwrap_or(""), 60),
        _ => compact(input, 60),
    };
    let res = match (name, result) {
        ("propose_def" | "rollback_def" | "propose_kernel", Ok(s)) => first_line(s, 60).split(':').next().unwrap_or("").to_string(),
        ("propose_def" | "rollback_def" | "propose_kernel", Err(_)) => "rejected".into(),
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
