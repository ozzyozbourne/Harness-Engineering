//! The wall: our Forth VM (`forth/vm`, compiled to wasm32) running inside
//! Wasmtime with fuel metering, a memory cap and a wall-clock backstop.
//!
//! Nothing that runs in the VM can crash or hang the host: every Forth error
//! comes back as a JSON report from the VM, and every Wasmtime trap (out of
//! fuel, epoch timeout, out-of-bounds access, unreachable, stack overflow,
//! memory cap) is caught here and turned into the same kind of report.
//!
//! The whole Forth image (dictionary, slots, code, data space, stacks) lives
//! in linear memory, so `snapshot` is a copy of linear memory and `restore`
//! is a fresh instance with that copy written back.

use super::state::{i64_field, str_field};
use super::tools;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::{Caller, Config, Engine, Extern, InstancePre, Linker, Memory, Module, ResourceLimiter, Store, Trap, TypedFunc};

/// Status codes and host word op codes shared with the VM (the same file is
/// compiled into the VM as `mod abi`).
#[path = "../../../vm/src/abi.rs"]
mod abi;
use abi::*;

pub const VM_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cmh_forth_vm.wasm"));

const TICK: Duration = Duration::from_millis(10);
const FILE_MAX: usize = 1 << 20;

/// Resource limits for one entry into the VM (a call, a definition).
#[derive(Clone, Debug)]
pub struct Limits {
    /// Wasm instructions (roughly) per entry.
    pub fuel: u64,
    /// Wall-clock backstop per entry.
    pub timeout: Duration,
    /// Cap on linear memory, bytes.
    pub memory: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { fuel: 100_000_000, timeout: Duration::from_secs(5), memory: 64 << 20 }
    }
}

/// Live userspace code the VM may compile lazily: name -> (version, source),
/// borrowed from the registry.
pub type Live<'a> = BTreeMap<&'a str, (u32, &'a str)>;

pub struct CallOutcome {
    pub result: Result<String, String>,
    /// A Wasmtime trap ended the call; the instance must be restored.
    pub trapped: bool,
    /// Definitions compiled lazily during the call.
    pub loads: Vec<(String, u32)>,
    /// Lessons added through the `remember` host word.
    pub lessons: Vec<String>,
}

/// A copy of the VM's linear memory: the whole Forth image.
pub struct Snapshot(Vec<u8>);

// A snapshot is never empty, so `len` (bytes, for traces) has no `is_empty`.
#[allow(clippy::len_without_is_empty)]
impl Snapshot {
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Per-instance host state (the Wasmtime store data).
struct Host {
    memory_cap: usize,
    /// Linear memory growth was refused by the cap during this entry.
    memory_hit: bool,
    /// A host word's result or error message, until the VM takes it.
    pending: Vec<u8>,
    /// Live userspace names the VM may bind lazily (`host_known`).
    known: HashSet<String>,
    /// The VM's panic message, if it panicked.
    panic: Option<String>,
    /// Lessons from the `remember` host word during this entry.
    lessons: Vec<String>,
    /// Canonical working directory: file host words stay inside it.
    work_dir: PathBuf,
    protected: Vec<PathBuf>,
}

impl ResourceLimiter for Host {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if desired > self.memory_cap {
            self.memory_hit = true;
            return Ok(false);
        }
        Ok(true)
    }
    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= 10_000)
    }
}

/// Shared by every VM instance: the engine (with its epoch ticker thread)
/// and the module, already linked against the host functions.
struct Runtime {
    engine: Engine,
    pre: InstancePre<Host>,
}

fn runtime() -> Result<&'static Runtime, String> {
    static RT: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        let mut c = Config::new();
        c.consume_fuel(true).epoch_interruption(true);
        let engine = Engine::new(&c).map_err(|e| format!("wasmtime engine: {e:?}"))?;
        let module = Module::new(&engine, VM_WASM).map_err(|e| format!("compile Forth VM module: {e:?}"))?;
        let pre = host_functions(&engine)
            .and_then(|linker| linker.instantiate_pre(&module))
            .map_err(|e| format!("link Forth VM: {e:?}"))?;
        // The epoch ticker drives every store's wall-clock deadline.
        let ticker = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(TICK);
                ticker.increment_epoch();
            }
        });
        Ok(Runtime { engine, pre })
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The functions the VM imports from module "cmh".
fn host_functions(engine: &Engine) -> wasmtime::Result<Linker<Host>> {
    let mut linker: Linker<Host> = Linker::new(engine);
    linker.func_wrap("cmh", "host_word", |mut c: Caller<'_, Host>, op: i32, a: i32, al: i32, b: i32, bl: i32| -> i64 {
        host_word(&mut c, op, a, al, b, bl)
    })?;
    linker.func_wrap("cmh", "host_take", |mut c: Caller<'_, Host>, dst: i32| {
        let data = std::mem::take(&mut c.data_mut().pending);
        if let Some(Extern::Memory(m)) = c.get_export("memory") {
            let _ = m.write(&mut c, dst as u32 as usize, &data);
        }
    })?;
    linker.func_wrap("cmh", "host_pending_len", |c: Caller<'_, Host>| -> i32 { c.data().pending.len() as i32 })?;
    linker.func_wrap("cmh", "host_known", |mut c: Caller<'_, Host>, p: i32, l: i32| -> i32 {
        let name = read(&mut c, p, l).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        i32::from(c.data().known.contains(&name))
    })?;
    linker.func_wrap("cmh", "host_panic", |mut c: Caller<'_, Host>, p: i32, l: i32| {
        let msg = read(&mut c, p, l).map(|b| String::from_utf8_lossy(&b).into_owned());
        c.data_mut().panic = msg;
    })?;
    Ok(linker)
}

/// The VM's exports (`cmh_*` in `forth/vm/src/lib.rs`), resolved once per instance.
struct Exports {
    init: TypedFunc<(), ()>,
    mailbox: TypedFunc<i32, i32>,
    out_ptr: TypedFunc<(), i32>,
    out_len: TypedFunc<(), i32>,
    define: TypedFunc<(i32, i32, i32), i32>,
    call: TypedFunc<(i32, i32), i32>,
    resume: TypedFunc<i32, i32>,
    bind: TypedFunc<(i32, i32), i32>,
    unbind: TypedFunc<i32, ()>,
    bound: TypedFunc<i32, i32>,
    /// `cmh_where`: a report for wherever execution was when a trap hit.
    locate: TypedFunc<(i32, i32), i32>,
    stats: TypedFunc<(), i64>,
}

impl Exports {
    fn resolve(inst: &wasmtime::Instance, store: &mut Store<Host>) -> WResult<Exports> {
        Ok(Exports {
            init: inst.get_typed_func(&mut *store, "cmh_init")?,
            mailbox: inst.get_typed_func(&mut *store, "cmh_mailbox")?,
            out_ptr: inst.get_typed_func(&mut *store, "cmh_out_ptr")?,
            out_len: inst.get_typed_func(&mut *store, "cmh_out_len")?,
            define: inst.get_typed_func(&mut *store, "cmh_define")?,
            call: inst.get_typed_func(&mut *store, "cmh_call")?,
            resume: inst.get_typed_func(&mut *store, "cmh_resume")?,
            bind: inst.get_typed_func(&mut *store, "cmh_bind")?,
            unbind: inst.get_typed_func(&mut *store, "cmh_unbind")?,
            bound: inst.get_typed_func(&mut *store, "cmh_bound")?,
            locate: inst.get_typed_func(&mut *store, "cmh_where")?,
            stats: inst.get_typed_func(&mut *store, "cmh_stats")?,
        })
    }
}

pub struct ForthVm {
    store: Store<Host>,
    mem: Memory,
    ex: Exports,
    limits: Limits,
}

type WResult<T> = wasmtime::Result<T>;

impl ForthVm {
    /// A brand-new VM with only the core words. `work_dir` must be canonical:
    /// file host words compare canonical paths against it.
    pub fn new(limits: Limits, work_dir: PathBuf, protected: Vec<PathBuf>) -> Result<ForthVm, String> {
        let mut vm = Self::instantiate(limits, work_dir, protected)?;
        vm.budget();
        vm.ex.init.call(&mut vm.store, ()).map_err(|e| format!("Forth VM init failed: {e:?}"))?;
        Ok(vm)
    }

    fn instantiate(limits: Limits, work_dir: PathBuf, protected: Vec<PathBuf>) -> Result<ForthVm, String> {
        let rt = runtime()?;
        let host = Host {
            memory_cap: limits.memory,
            memory_hit: false,
            pending: Vec::new(),
            known: HashSet::new(),
            panic: None,
            lessons: Vec::new(),
            work_dir,
            protected,
        };
        let mut store = Store::new(&rt.engine, host);
        store.limiter(|h| h);
        store.epoch_deadline_trap();
        let inst = rt.pre.instantiate(&mut store).map_err(|e| format!("instantiate Forth VM: {e:?}"))?;
        let mem = inst.get_memory(&mut store, "memory").ok_or("Forth VM exports no memory")?;
        let ex = Exports::resolve(&inst, &mut store).map_err(|e| format!("instantiate Forth VM: {e:?}"))?;
        Ok(ForthVm { store, mem, ex, limits })
    }

    /// Fresh fuel and wall-clock deadline for one entry into the VM.
    fn budget(&mut self) {
        let _ = self.store.set_fuel(self.limits.fuel);
        let ticks = (self.limits.timeout.as_millis() / TICK.as_millis()).max(1) as u64;
        self.store.set_epoch_deadline(ticks);
    }

    /// Writes `parts` back to back into the VM's mailbox: the arguments of
    /// the next export call, which gets their lengths.
    fn put_mail(&mut self, parts: &[&[u8]]) -> WResult<()> {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let mut at = self.ex.mailbox.call(&mut self.store, total as i32)? as u32 as usize;
        for part in parts {
            self.mem.write(&mut self.store, at, part)?;
            at += part.len();
        }
        Ok(())
    }

    /// The VM's output buffer: a result, a report or a name, by status.
    fn out(&mut self) -> WResult<Vec<u8>> {
        let p = self.ex.out_ptr.call(&mut self.store, ())? as u32 as usize;
        let l = self.ex.out_len.call(&mut self.store, ())? as u32 as usize;
        let mut buf = vec![0u8; l];
        self.mem.read(&self.store, p, &mut buf)?;
        Ok(buf)
    }

    fn reset_host(&mut self, live: &Live) {
        let h = self.store.data_mut();
        h.known = live.keys().map(|k| k.to_string()).collect();
        h.panic = None;
        h.memory_hit = false;
        h.pending.clear();
        h.lessons.clear();
    }

    /// Compiles `source` as `name` version `version` and binds the public
    /// slot `name` to its `run`. Errors are formatted reports.
    pub fn define(&mut self, name: &str, version: u32, source: &str, live: &Live) -> Result<(), String> {
        self.reset_host(live);
        self.budget();
        let r = self.define_inner(name, version, source);
        match r {
            Ok(None) => Ok(()),
            Ok(Some(report)) => Err(format_report(&report, b"")),
            Err(e) => Err(self.trap_report(&e, b"")),
        }
    }

    /// `Some(report)` if the source did not compile.
    fn define_inner(&mut self, name: &str, version: u32, source: &str) -> WResult<Option<Vec<u8>>> {
        self.put_mail(&[name.as_bytes(), source.as_bytes()])?;
        let st = self.ex.define.call(&mut self.store, (name.len() as i32, source.len() as i32, version as i32))?;
        Ok(if st == ST_OK { None } else { Some(self.out()?) })
    }

    /// Calls the `run` of definition `name` with `input` (JSON text).
    /// Unbound definitions found in `live` are compiled on first use.
    pub fn call(&mut self, name: &str, input: &[u8], live: &Live) -> CallOutcome {
        self.reset_host(live);
        self.budget();
        let mut loads = Vec::new();
        let r = self.call_inner(name, input, live, &mut loads);
        let lessons = std::mem::take(&mut self.store.data_mut().lessons);
        match r {
            Ok(result) => CallOutcome { result, trapped: false, loads, lessons },
            Err(e) => CallOutcome { result: Err(self.trap_report(&e, input)), trapped: true, loads, lessons },
        }
    }

    fn call_inner(&mut self, name: &str, input: &[u8], live: &Live, loads: &mut Vec<(String, u32)>) -> WResult<Result<String, String>> {
        self.put_mail(&[name.as_bytes(), input])?;
        let mut st = self.ex.call.call(&mut self.store, (name.len() as i32, input.len() as i32))?;
        loop {
            let out = self.out()?;
            match st {
                ST_OK => return Ok(Ok(String::from_utf8_lossy(&out).into_owned())),
                ST_ERR => return Ok(Err(format_report(&out, input))),
                // The VM hit an unbound public slot: compile that definition
                // (if it is live) and let the VM continue or fail.
                ST_NEED => {
                    let need = String::from_utf8_lossy(&out).into_owned();
                    let ok = match live.get(need.as_str()) {
                        Some((v, src)) => {
                            if let Some(rep) = self.define_inner(&need, *v, src)? {
                                return Ok(Err(format!(
                                    "lazy load of `{need}` v{v} failed:\n{}",
                                    format_report(&rep, b"")
                                )));
                            }
                            loads.push((need.clone(), *v));
                            1
                        }
                        None => 0,
                    };
                    st = self.ex.resume.call(&mut self.store, ok)?;
                }
                other => return Ok(Err(format!("Forth VM returned unknown status {other}"))),
            }
        }
    }

    /// Fresh budget, `name` in the mailbox, then the export `f` with
    /// `params`: for the exports that take a definition name.
    fn call_named<P: wasmtime::WasmParams, R: wasmtime::WasmResults>(
        &mut self,
        name: &str,
        f: fn(&Exports) -> &TypedFunc<P, R>,
        params: P,
    ) -> WResult<R> {
        self.budget();
        self.put_mail(&[name.as_bytes()])?;
        f(&self.ex).call(&mut self.store, params)
    }

    /// Version bound to the public slot `name` (0 if not compiled/bound).
    pub fn bound(&mut self, name: &str) -> u32 {
        self.call_named(name, |ex| &ex.bound, name.len() as i32).map_or(0, |v| v.max(0) as u32)
    }

    /// Points `name` at an already compiled version. False if not compiled.
    pub fn bind(&mut self, name: &str, version: u32) -> bool {
        self.call_named(name, |ex| &ex.bind, (name.len() as i32, version as i32)).is_ok_and(|v| v == 1)
    }

    /// Unbinds `name`; its next call compiles the live version lazily.
    pub fn unbind(&mut self, name: &str) {
        let _ = self.call_named(name, |ex| &ex.unbind, name.len() as i32);
    }

    /// Bytes of Forth data space used and number of compiled words.
    pub fn stats(&mut self) -> (u64, u64) {
        self.budget();
        let v = self.ex.stats.call(&mut self.store, ()).unwrap_or(0);
        ((v >> 32) as u64, (v & 0xffff_ffff) as u64)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot(self.mem.data(&self.store).to_vec())
    }

    /// Replaces this instance with a fresh one whose linear memory is the
    /// snapshot. Globals (the shadow stack pointer) start at their initial
    /// values, which are the values between top-level calls.
    pub fn restore(&mut self, snap: &Snapshot) -> Result<(), String> {
        let h = self.store.data();
        let mut fresh = Self::instantiate(self.limits.clone(), h.work_dir.clone(), h.protected.clone())?;
        let cur = fresh.mem.data_size(&fresh.store);
        if snap.0.len() > cur {
            let pages = (snap.0.len() - cur).div_ceil(65536) as u64;
            fresh.mem.grow(&mut fresh.store, pages).map_err(|e| format!("restore: grow memory: {e:?}"))?;
        }
        fresh.mem.data_mut(&mut fresh.store)[..snap.0.len()].copy_from_slice(&snap.0);
        *self = fresh;
        Ok(())
    }

    /// Turns a Wasmtime error (trap) into a report, asking the VM where it
    /// was when the trap hit.
    fn trap_report(&mut self, e: &wasmtime::Error, input: &[u8]) -> String {
        let h = self.store.data();
        let (kind, msg) = match e.downcast_ref::<Trap>() {
            _ if h.memory_hit => (
                "memory-limit",
                format!("linear memory would exceed the {} MB cap", self.limits.memory >> 20),
            ),
            Some(Trap::OutOfFuel) => (
                "fuel-exhausted",
                format!(
                    "ran out of fuel ({} wasm instructions) without finishing: probably an infinite loop",
                    self.limits.fuel
                ),
            ),
            Some(Trap::Interrupt) => (
                "timeout",
                format!("wall-clock limit of {:.1}s exceeded", self.limits.timeout.as_secs_f64()),
            ),
            Some(Trap::MemoryOutOfBounds) => (
                "wasm-out-of-bounds",
                "WebAssembly trap: out-of-bounds memory access (an address past the end of linear memory)".to_string(),
            ),
            Some(Trap::StackOverflow) => ("wasm-stack-overflow", "WebAssembly trap: call stack exhausted".to_string()),
            Some(Trap::UnreachableCodeReached) if h.panic.is_some() => {
                ("vm-panic", format!("the Forth VM panicked: {}", h.panic.clone().unwrap_or_default()))
            }
            Some(t) => ("wasm-trap", format!("WebAssembly trap: {t}")),
            None => ("wasm-error", format!("{e:?}").lines().next().unwrap_or("").to_string()),
        };
        // Small fresh budget for the report; if that traps too, fall back.
        let _ = self.store.set_fuel(20_000_000);
        self.store.set_epoch_deadline(100);
        let loc: WResult<Vec<u8>> = (|| {
            self.put_mail(&[kind.as_bytes(), msg.as_bytes()])?;
            self.ex.locate.call(&mut self.store, (kind.len() as i32, msg.len() as i32))?;
            self.out()
        })();
        match loc {
            Ok(json) => format_report(&json, input),
            Err(_) => {
                let v = serde_json::json!({"kind": kind, "message": msg});
                format_report(v.to_string().as_bytes(), input)
            }
        }
    }
}

/// The guest bytes at (p, l), if they are inside linear memory.
fn guest_slice(data: &[u8], p: i32, l: i32) -> Option<&[u8]> {
    let (p, l) = (p as u32 as usize, l as u32 as usize);
    data.get(p..p.checked_add(l)?)
}

fn read(c: &mut Caller<'_, Host>, p: i32, l: i32) -> Option<Vec<u8>> {
    let Some(Extern::Memory(m)) = c.get_export("memory") else { return None };
    guest_slice(m.data(&*c), p, l).map(<[u8]>::to_vec)
}

enum HostOut {
    Str(Vec<u8>),
    Num(i64),
}

fn host_word(c: &mut Caller<'_, Host>, op: i32, a: i32, al: i32, b: i32, bl: i32) -> i64 {
    const OUTSIDE: &str = "string arguments are outside linear memory";
    // The arguments are read in place: guest memory and host state are
    // borrowed side by side, without copying the strings.
    let r = match c.get_export("memory") {
        Some(Extern::Memory(m)) => {
            let (data, host) = m.data_and_store_mut(&mut *c);
            match (guest_slice(data, a, al), guest_slice(data, b, bl)) {
                (Some(x), Some(y)) => host_op(host, op, x, y),
                _ => Err(OUTSIDE.into()),
            }
        }
        _ => Err(OUTSIDE.into()),
    };
    match r {
        Ok(HostOut::Str(s)) => {
            let n = s.len() as i64;
            c.data_mut().pending = s;
            n
        }
        Ok(HostOut::Num(n)) => n.max(HOST_ERR + 1),
        Err(e) => {
            c.data_mut().pending = e.into_bytes();
            HOST_ERR
        }
    }
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// `a.b.0.c`: object keys and array indexes separated by dots; "" is the root.
fn lookup<'a>(root: &'a Value, path: &str) -> Result<&'a Value, String> {
    let mut cur = root;
    if path.is_empty() {
        return Ok(cur);
    }
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg).ok_or_else(|| {
                let keys: Vec<&String> = m.keys().take(20).collect();
                format!("key `{seg}` not found (path `{path}`); keys here: {keys:?}")
            })?,
            Value::Array(a) => {
                let i: usize = seg
                    .parse()
                    .map_err(|_| format!("`{seg}` is not an index into an array of {} (path `{path}`)", a.len()))?;
                a.get(i).ok_or_else(|| format!("index {i} out of range: the array has {} item(s) (path `{path}`)", a.len()))?
            }
            other => return Err(format!("cannot index {} with `{seg}` (path `{path}`)", json_type(other))),
        };
    }
    Ok(cur)
}

fn parse_json(x: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(x).map_err(|e| {
        let s = String::from_utf8_lossy(&x[..x.len().min(80)]).into_owned();
        format!("the JSON argument is not valid JSON ({e}): {s:?}")
    })
}

fn checked_path(h: &Host, p: &[u8]) -> Result<PathBuf, String> {
    let raw = String::from_utf8_lossy(p).into_owned();
    if raw.split(['/', '\\']).any(|c| c == "..") {
        return Err(format!("refused `{raw}`: `..` is not allowed; paths stay inside the working directory"));
    }
    let path = tools::resolve(&h.work_dir, &raw);
    if !path.starts_with(&h.work_dir) {
        return Err(format!("refused `{raw}`: outside the working directory {}", h.work_dir.display()));
    }
    if let Some(p) = tools::protected_by(&path, &h.protected) {
        return Err(format!("refused `{raw}`: inside protected path {}", p.display()));
    }
    Ok(path)
}

fn host_op(h: &mut Host, op: i32, x: &[u8], y: &[u8]) -> Result<HostOut, String> {
    match op {
        // json-get ( json path -- str )
        OP_JSON_GET => {
            let v = parse_json(x)?;
            let path = String::from_utf8_lossy(y);
            Ok(HostOut::Str(match lookup(&v, &path)? {
                Value::String(s) => s.clone().into_bytes(),
                other => other.to_string().into_bytes(),
            }))
        }
        // json-len ( json path -- n ): -1 if missing
        OP_JSON_LEN => {
            let v = parse_json(x)?;
            let path = String::from_utf8_lossy(y);
            Ok(HostOut::Num(match lookup(&v, &path) {
                Err(_) => -1,
                Ok(Value::Array(a)) => a.len() as i64,
                Ok(Value::Object(m)) => m.len() as i64,
                Ok(Value::String(s)) => s.len() as i64,
                Ok(_) => 0,
            }))
        }
        // json-str ( str -- "json string" )
        OP_JSON_STR => Ok(HostOut::Str(Value::String(String::from_utf8_lossy(x).into_owned()).to_string().into_bytes())),
        // file-read ( path -- contents )
        OP_FILE_READ => {
            let path = checked_path(h, x)?;
            // Refuse a big file before reading it. A failed stat is left to
            // the read, so its error message is the read's.
            if let Ok(meta) = std::fs::metadata(&path)
                && meta.len() > FILE_MAX as u64
            {
                return Err(too_big(&path, meta.len()));
            }
            let data = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            // Files whose size stat does not report (e.g. under /proc).
            if data.len() > FILE_MAX {
                return Err(too_big(&path, data.len() as u64));
            }
            Ok(HostOut::Str(data))
        }
        // file-write ( contents path -- )
        OP_FILE_WRITE => {
            let path = checked_path(h, y)?;
            tools::write_creating_dirs(&path, x)?;
            Ok(HostOut::Num(0))
        }
        // remember ( text -- )
        OP_REMEMBER => {
            let t = String::from_utf8_lossy(x).trim().to_string();
            if t.is_empty() {
                return Err("empty lesson text".into());
            }
            if h.lessons.len() < 10 {
                h.lessons.push(t);
            }
            Ok(HostOut::Num(0))
        }
        other => Err(format!("unknown host word {other}")),
    }
}

fn too_big(path: &Path, len: u64) -> String {
    format!("{} is {len} bytes; file-read is limited to {FILE_MAX}", path.display())
}

fn hint(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "stack-underflow" => "check each word's ( before -- after ) effect against what is really on the stack at that point",
        "stack-overflow" => "a loop pushes more than it drops; every iteration must leave the stack as it found it",
        "undefined-word" => "define helper words before `run` in the same source, or call a live userspace definition by its name",
        "fuel-exhausted" | "timeout" => {
            "a loop never terminates: make sure BEGIN ... UNTIL / WHILE conditions eventually change and that /string or counters advance"
        }
        "bad-address" | "wasm-out-of-bounds" => {
            "use only addresses from HERE, VARIABLE, CREATE and strings; check that ( addr len ) pairs are not swapped"
        }
        "contract" => "`run` receives ( json-addr json-len ) and must leave exactly ( out-addr out-len ), e.g. from S\" or B>",
        "division-by-zero" => "guard the divisor with `dup 0= if ... then`",
        "host-error" => "for optional JSON fields test `json-len` first (-1 means missing)",
        _ => return None,
    })
}

/// Formats a VM error report (JSON) as the text the model sees.
pub fn format_report(json: &[u8], input: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(json) else {
        return format!("forth error: {}", String::from_utf8_lossy(json));
    };
    let (kind, word, def, token) = (str_field(&v, "kind"), str_field(&v, "word"), str_field(&v, "def"), str_field(&v, "token"));
    let line = i64_field(&v, "line");
    let mut out = format!("forth error [{kind}]");
    if !word.is_empty() {
        out.push_str(&format!(" in word `{word}`"));
    }
    if !def.is_empty() {
        out.push_str(&format!(" of {def} v{}", i64_field(&v, "version")));
    }
    if line > 0 {
        out.push_str(&format!(", line {line}"));
    }
    if !token.is_empty() {
        out.push_str(&format!(", at `{token}`"));
    }
    out.push_str(&format!(":\n  {}", str_field(&v, "message")));
    if kind != "compile-error" && v.get("stack").is_some() {
        let items: Vec<String> = v["stack"].as_array().into_iter().flatten().map(|x| x.to_string()).collect();
        let depth = i64_field(&v, "depth");
        let more = if depth as usize > items.len() { "... " } else { "" };
        out.push_str(&format!("\n  data stack (depth {depth}, top last): [{more}{}]", items.join(" ")));
    }
    let trace: Vec<String> = v["trace"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
    if !trace.is_empty() {
        out.push_str(&format!("\n  called from: {}", trace.join(" <- ")));
    }
    if let Some(t) = v.get("top_string").and_then(Value::as_str) {
        out.push_str(&format!("\n  top two items as a string: {t:?}"));
    }
    let output = str_field(&v, "output");
    if !output.is_empty() {
        out.push_str(&format!("\n  output before the error: {:?}", tools::clip(&output, 300)));
    }
    if !input.is_empty() {
        out.push_str(&format!("\n  input: {}", tools::clip(&String::from_utf8_lossy(input), 300)));
    }
    if let Some(h) = hint(&kind) {
        out.push_str(&format!("\n  hint: {h}"));
    }
    out
}
