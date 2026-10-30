//! Running a kernel image: one Wasmtime component instance per store, with
//! the WIT imports implemented by `HostState`.
//!
//! Limits, all enforced here and never by the guest:
//! - guest time: every cmh import records the time it spends in the host
//!   (model requests, bash, image builds do not count as guest time). The
//!   epoch callback, run every 10 ms of guest execution, traps a call that
//!   has run `slice` without calling the host, or `total` in one export call;
//! - memory: linear memory may not grow past `memory`;
//! - every trap (panic, limit, out of bounds, stack overflow) becomes a
//!   `GuestFault` with a readable report. A trapped instance is never used
//!   again: the hypervisor boots a fresh one and the journal restores state.

use super::journal::Journal;
use super::provider::{Provider, request_body};
use super::sandbox;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store, Trap, UpdateDeadline};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({ world: "harness", path: "../guest/wit" });
}

use bindings::Harness;
pub use bindings::cmh::harness::types::{BashOutput, TaskOutcome as GuestOutcome};

const TICK: Duration = Duration::from_millis(10);
const STDERR_CAP: usize = 64 * 1024;

/// Resource limits for a kernel image instance.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Longest stretch of guest code without a host call.
    pub slice: Duration,
    /// Guest time per export call (host calls excluded).
    pub total: Duration,
    /// Cap on linear memory, bytes.
    pub memory: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { slice: Duration::from_secs(20), total: Duration::from_secs(300), memory: 512 << 20 }
    }
}

/// The engine (with its epoch ticker thread) and the linker, shared by all
/// instances.
pub struct Runtime {
    pub engine: Engine,
    linker: Linker<HostState>,
}

pub fn runtime() -> Result<&'static Runtime, String> {
    static RT: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        let mut c = Config::new();
        c.epoch_interruption(true);
        let engine = Engine::new(&c).map_err(|e| format!("wasmtime engine: {e:?}"))?;
        let mut linker: Linker<HostState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| format!("link WASI: {e:?}"))?;
        Harness::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s).map_err(|e| format!("link cmh host: {e:?}"))?;
        let ticker = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(TICK);
                ticker.increment_epoch();
            }
        });
        Ok(Runtime { engine, linker })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Compiles a component, using a native-code cache in `cache_dir` (inside
/// the state dir, which bash cannot write).
pub fn load_component(wasm: &[u8], cache_dir: &Path) -> Result<Component, String> {
    let rt = runtime()?;
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        wasm.hash(&mut h);
        env!("CARGO_PKG_VERSION").hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let cached = cache_dir.join(format!("{key}.cwasm"));
    if cached.exists() {
        // SAFETY: the file was written by `Component::serialize` below, in
        // the state dir, which the sandboxed bash cannot modify.
        if let Ok(c) = unsafe { Component::deserialize_file(&rt.engine, &cached) } {
            return Ok(c);
        }
    }
    let c = Component::new(&rt.engine, wasm).map_err(|e| format!("not a valid cmh component: {}", first_line(&format!("{e:#}"))))?;
    if let Ok(bytes) = c.serialize() {
        let _ = std::fs::create_dir_all(cache_dir);
        let tmp = cached.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &cached);
        }
    }
    Ok(c)
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
}

/// What a guest may propose to the image manager (absent in scratch instances).
pub type ProposeFn = Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>;
pub type ImagesFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Everything an instance's imports act on.
pub struct Env {
    pub state_dir: PathBuf,
    pub work_dir: PathBuf,
    /// The hypervisor's source tree (read-only preopen, protected).
    pub kernel_dir: PathBuf,
    pub protected: Vec<PathBuf>,
    pub base_prompt: String,
    pub soak: u32,
    pub reflect: bool,
    pub max_turns: usize,
    pub echo_text: bool,
    pub journal: Arc<Mutex<Journal>>,
    pub provider: Arc<Mutex<Box<dyn Provider>>>,
    pub propose: Option<ProposeFn>,
    pub images: Option<ImagesFn>,
    /// Allow `bash` (false for scratch instances that test a candidate).
    pub allow_bash: bool,
}

pub struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    env: Arc<Env>,
    image: u32,
    limits: Limits,
    memory_hit: bool,
    /// (kind, message) when a time limit stopped the guest.
    limit_hit: Option<(String, String)>,
    /// When the current export call started, and time spent in cmh imports since.
    call_start: Instant,
    host_time: Duration,
    /// When the guest last returned from a cmh import (or was called).
    slice_start: Instant,
    stderr: MemoryOutputPipe,
    /// The latest task checkpoint from the guest ("{}" clears it).
    pub checkpoint: Option<String>,
    /// The guest left the base prompt out of a request; the host put it back.
    pub base_prompt_inserted: bool,
}

impl ResourceLimiter for HostState {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if desired > self.limits.memory {
            self.memory_hit = true;
            return Ok(false);
        }
        Ok(true)
    }
    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= 100_000)
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl HostState {
    fn new(env: Arc<Env>, image: u32, limits: Limits) -> Result<HostState, String> {
        let stderr = MemoryOutputPipe::new(STDERR_CAP);
        let mut b = WasiCtxBuilder::new();
        b.stderr(stderr.clone());
        // Read-only views with the same paths as on the host.
        let mut dirs = vec![env.work_dir.clone(), env.state_dir.clone()];
        if env.kernel_dir.is_dir() {
            dirs.push(env.kernel_dir.clone());
        }
        for d in dirs {
            let guest_path = d.to_string_lossy().into_owned();
            b.preopened_dir(&d, guest_path, FsPerms::ReadOnly).map_err(|e| format!("preopen {}: {e}", d.display()))?;
        }
        Ok(HostState {
            wasi: b.build(),
            table: ResourceTable::new(),
            env,
            image,
            limits,
            memory_hit: false,
            limit_hit: None,
            call_start: Instant::now(),
            host_time: Duration::ZERO,
            slice_start: Instant::now(),
            stderr,
            checkpoint: None,
            base_prompt_inserted: false,
        })
    }

    /// Runs one import: its time is host time, and the guest's slice
    /// starts again when it returns.
    fn host<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let t = Instant::now();
        let r = f(self);
        self.host_time += t.elapsed();
        self.slice_start = Instant::now();
        r
    }

    fn stderr_tail(&self) -> String {
        let b = self.stderr.contents();
        let s = String::from_utf8_lossy(&b);
        let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
        lines[lines.len().saturating_sub(6)..].join("\n    ")
    }
}

impl bindings::cmh::harness::types::Host for HostState {}

/// The cmh imports. Each is called through `HostState::host` (below).
impl HostState {
    fn do_log(&mut self, line: String) {
        eprintln!("{line}");
    }

    fn do_print(&mut self, text: String) {
        println!("{text}");
    }

    fn do_now_ms(&mut self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
    }

    fn do_now_iso(&mut self) -> String {
        super::journal::now_iso()
    }

    fn do_config(&mut self) -> String {
        let e = &self.env;
        json!({
            "state-dir": e.state_dir, "work-dir": e.work_dir, "kernel-dir": e.kernel_dir,
            "soak": e.soak, "reflect": e.reflect, "max-turns": e.max_turns,
            "base-prompt": e.base_prompt, "image": self.image, "echo-text": e.echo_text,
        })
        .to_string()
    }

    fn do_post_messages(&mut self, body: String) -> Result<String, String> {
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("post-messages: body is not JSON: {e}"))?;
        let mut system: Vec<String> = v["system"]
            .as_array()
            .ok_or("post-messages: `system` must be an array of strings")?
            .iter()
            .map(|s| s.as_str().map(String::from).or_else(|| s["text"].as_str().map(String::from)).unwrap_or_default())
            .collect();
        // The base prompt is immutable: whatever the image sends, it comes first.
        if system.first() != Some(&self.env.base_prompt) {
            system.retain(|s| s != &self.env.base_prompt);
            system.insert(0, self.env.base_prompt.clone());
            self.base_prompt_inserted = true;
        }
        let messages = v.get("messages").cloned().ok_or("post-messages: `messages` is required")?;
        let tools = v["tools"].as_array().cloned().unwrap_or_default();
        let mut p = self.env.provider.lock().unwrap_or_else(|e| e.into_inner());
        let req = request_body(p.model(), &system, &messages, &tools, p.oauth());
        p.complete(&req).map(|r| r.to_string())
    }

    fn do_bash(&mut self, command: String, timeout_s: u32) -> Result<BashOutput, String> {
        if !self.env.allow_bash {
            return Err("bash is not available while an image is being tested".into());
        }
        let r = sandbox::bash(&self.env.work_dir, &command, timeout_s, &self.env.protected)?;
        Ok(BashOutput { output: r.output, exit_code: r.exit_code, timed_out: r.timed_out })
    }

    fn do_write_file(&mut self, path: String, content: String) -> Result<String, String> {
        sandbox::write_file(&self.env.work_dir, &path, &content, &self.env.protected)
    }

    fn do_journal_append(&mut self, event: String) -> Result<String, String> {
        let v: Value = serde_json::from_str(&event).map_err(|e| format!("journal: event is not JSON: {e}"))?;
        let mut j = self.env.journal.lock().unwrap_or_else(|e| e.into_inner());
        j.append_guest(&v, self.image).map(|ev| ev.to_string())
    }

    fn do_journal_read(&mut self) -> Vec<String> {
        let j = self.env.journal.lock().unwrap_or_else(|e| e.into_inner());
        j.read_all().unwrap_or_default().iter().map(Value::to_string).collect()
    }

    fn do_checkpoint(&mut self, state: String) {
        self.checkpoint = if state.trim() == "{}" { None } else { Some(state) };
    }

    fn do_propose_image(&mut self, request: String) -> Result<String, String> {
        let Some(f) = self.env.propose.clone() else {
            return Err("propose_kernel is not available while an image is being tested".into());
        };
        let v: Value = serde_json::from_str(&request).map_err(|e| format!("propose_kernel: request is not JSON: {e}"))?;
        f(&v)
    }

    fn do_images(&mut self) -> String {
        self.env.images.as_ref().map_or("(no image history in a scratch instance)".into(), |f| f())
    }
}

impl bindings::cmh::harness::host::Host for HostState {
    fn log(&mut self, line: String) {
        self.host(|s| s.do_log(line))
    }

    fn print(&mut self, text: String) {
        self.host(|s| s.do_print(text))
    }

    fn now_ms(&mut self) -> u64 {
        self.host(|s| s.do_now_ms())
    }

    fn now_iso(&mut self) -> String {
        self.host(|s| s.do_now_iso())
    }

    fn config(&mut self) -> String {
        self.host(|s| s.do_config())
    }

    fn post_messages(&mut self, body: String) -> Result<String, String> {
        self.host(|s| s.do_post_messages(body))
    }

    fn bash(&mut self, command: String, timeout_s: u32) -> Result<BashOutput, String> {
        self.host(|s| s.do_bash(command, timeout_s))
    }

    fn write_file(&mut self, path: String, content: String) -> Result<String, String> {
        self.host(|s| s.do_write_file(path, content))
    }

    fn journal_append(&mut self, event: String) -> Result<String, String> {
        self.host(|s| s.do_journal_append(event))
    }

    fn journal_read(&mut self) -> Vec<String> {
        self.host(|s| s.do_journal_read())
    }

    fn checkpoint(&mut self, state: String) {
        self.host(|s| s.do_checkpoint(state))
    }

    fn propose_image(&mut self, request: String) -> Result<String, String> {
        self.host(|s| s.do_propose_image(request))
    }

    fn images(&mut self) -> String {
        self.host(|s| s.do_images())
    }
}

/// A trap or limit hit, turned into a report for the model and the trace.
#[derive(Debug, Clone)]
pub struct GuestFault {
    pub kind: String,
    pub report: String,
}

fn hint(kind: &str) -> &'static str {
    match kind {
        "guest-timeout" | "guest-time-limit" => {
            "the kernel code loops without returning to the hypervisor; look for a loop whose exit condition never changes"
        }
        "memory-limit" => "the kernel allocates without bound (a growing Vec or String in a loop?)",
        "guest-panic" => "a Rust panic in the kernel: an unwrap/expect on None or Err, an index out of range, or an explicit panic!",
        "wasm-stack-overflow" => "unbounded recursion in the kernel",
        _ => "the kernel image hit a WebAssembly trap",
    }
}

/// Classifies a trap: a time or memory limit, a panic (from the guest's
/// stderr), or a WebAssembly trap.
fn fault_of(d: &HostState, e: &wasmtime::Error, export: &str, image: u32) -> GuestFault {
    let stderr = d.stderr_tail();
    let (kind, msg) = if let Some((k, m)) = &d.limit_hit {
        (k.clone(), m.clone())
    } else if d.memory_hit {
        ("memory-limit".to_string(), format!("linear memory would exceed the {} MB cap", d.limits.memory >> 20))
    } else {
        match e.downcast_ref::<Trap>() {
            Some(Trap::UnreachableCodeReached) if stderr.contains("panicked") => ("guest-panic".into(), "the kernel panicked".into()),
            Some(Trap::UnreachableCodeReached) if stderr.contains("memory allocation") => {
                ("memory-limit".into(), "the kernel ran out of memory".into())
            }
            Some(Trap::StackOverflow) => ("wasm-stack-overflow".into(), "WebAssembly trap: call stack exhausted".into()),
            Some(Trap::MemoryOutOfBounds) => ("wasm-out-of-bounds".into(), "WebAssembly trap: out-of-bounds memory access".into()),
            Some(t) => ("wasm-trap".into(), format!("WebAssembly trap: {t}")),
            None => ("wasm-error".into(), first_line(&format!("{e:#}"))),
        }
    };
    let mut report = format!("kernel trap [{kind}] in image v{} during {export}: {msg}", image);
    if !stderr.is_empty() {
        report.push_str(&format!("\n  guest stderr:\n    {stderr}"));
    }
    report.push_str(&format!("\n  hint: {}", hint(&kind)));
    GuestFault { kind, report }
}

/// One running kernel image.
pub struct Instance {
    store: Store<HostState>,
    harness: Harness,
    pub image: u32,
    /// The instance trapped; it must not be used again.
    pub dead: bool,
}

impl Instance {
    /// Instantiates `component` (image `image`) and runs its `init`.
    pub fn boot(component: &Component, image: u32, env: Arc<Env>, limits: Limits) -> Result<Instance, GuestFault> {
        let rt = runtime().map_err(|e| GuestFault { kind: "host-error".into(), report: e })?;
        let state = HostState::new(env, image, limits).map_err(|e| GuestFault { kind: "host-error".into(), report: e })?;
        let mut store = Store::new(&rt.engine, state);
        store.limiter(|s| s);
        store.epoch_deadline_callback(|mut s| {
            let d = s.data_mut();
            let slice = d.slice_start.elapsed();
            let total = d.call_start.elapsed().saturating_sub(d.host_time);
            if slice > d.limits.slice {
                let msg = format!(
                    "the kernel ran {:.1} s without returning to the hypervisor (limit {:.0} s)",
                    slice.as_secs_f64(),
                    d.limits.slice.as_secs_f64()
                );
                d.limit_hit = Some(("guest-timeout".into(), msg));
                return Err(wasmtime::format_err!("guest time limit"));
            }
            if total > d.limits.total {
                let msg = format!(
                    "the kernel used {:.1} s of guest time in one call (limit {:.0} s)",
                    total.as_secs_f64(),
                    d.limits.total.as_secs_f64()
                );
                d.limit_hit = Some(("guest-time-limit".into(), msg));
                return Err(wasmtime::format_err!("guest time limit"));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let harness = match Harness::instantiate(&mut store, component, &rt.linker) {
            Ok(h) => h,
            Err(e) => return Err(fault_of(store.data(), &e, "instantiate", image)),
        };
        let mut inst = Instance { store, harness, image, dead: false };
        inst.begin();
        match inst.harness.call_init(&mut inst.store) {
            Ok(Ok(())) => Ok(inst),
            Ok(Err(e)) => Err(GuestFault { kind: "init-error".into(), report: format!("image v{image} init failed: {e}") }),
            Err(e) => Err(inst.fault(e, "init")),
        }
    }

    /// Resets the per-call guest time budget.
    fn begin(&mut self) {
        let d = self.store.data_mut();
        d.call_start = Instant::now();
        d.slice_start = d.call_start;
        d.host_time = Duration::ZERO;
        d.limit_hit = None;
        d.memory_hit = false;
    }

    fn fault(&mut self, e: wasmtime::Error, export: &str) -> GuestFault {
        self.dead = true;
        fault_of(self.store.data(), &e, export, self.image)
    }

    fn call<R>(&mut self, export: &str, f: impl FnOnce(&Harness, &mut Store<HostState>) -> wasmtime::Result<R>) -> Result<R, GuestFault> {
        if self.dead {
            return Err(GuestFault { kind: "dead".into(), report: "the instance already trapped".into() });
        }
        self.begin();
        match f(&self.harness, &mut self.store) {
            Ok(r) => Ok(r),
            Err(e) => Err(self.fault(e, export)),
        }
    }

    pub fn run_task(&mut self, task: &str) -> Result<GuestOutcome, GuestFault> {
        self.call("run-task", |h, s| h.call_run_task(s, task))
    }

    pub fn resume_task(&mut self, checkpoint: &str, report: &str) -> Result<GuestOutcome, GuestFault> {
        self.call("resume-task", |h, s| h.call_resume_task(s, checkpoint, report))
    }

    pub fn refine(&mut self) -> Result<GuestOutcome, GuestFault> {
        self.call("refine", |h, s| h.call_refine(s))
    }

    pub fn command(&mut self, line: &str) -> Result<Result<String, String>, GuestFault> {
        self.call("command", |h, s| h.call_command(s, line))
    }

    pub fn self_test(&mut self) -> Result<Result<String, String>, GuestFault> {
        self.call("self-test", |h, s| h.call_self_test(s))
    }

    /// The checkpoint of the task in flight, if any.
    pub fn take_checkpoint(&mut self) -> Option<String> {
        self.store.data_mut().checkpoint.take()
    }

    pub fn base_prompt_inserted(&self) -> bool {
        self.store.data().base_prompt_inserted
    }
}
