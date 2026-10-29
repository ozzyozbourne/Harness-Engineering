//! Userspace code: each definition version becomes a tiny generated cdylib
//! crate, compiled by `cargo` and hot-loaded with `libloading`.
//!
//! Generated crate (`<state>/build/<name>_v<N>/`):
//! - `src/user.rs`: the agent's source, verbatim (compiler errors point here).
//! - `src/lib.rs`: a kernel-written wrapper that exports C-ABI functions taking
//!   and returning JSON strings, frees its own allocations, and runs the user
//!   function under `catch_unwind` so panics become errors.
//!
//! Libraries are never unloaded (old versions are leaked on purpose), so
//! function pointers stay valid for the life of the process.

use libloading::Library;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Bumped whenever the generated wrapper changes, so cached libraries rebuild.
const WRAPPER_ABI: u32 = 1;

type CallFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type FreeFn = unsafe extern "C" fn(*mut c_char);

#[derive(Clone, Copy)]
pub struct Loaded {
    call: CallFn,
    free: FreeFn,
}

pub struct Userspace {
    build_root: PathBuf,
    target_dir: PathBuf,
    loaded: HashMap<PathBuf, Loaded>,
    /// Wall time of the most recent cargo invocation, for tracing.
    pub last_build: Option<Duration>,
}

/// The entry point a definition must provide, decided by kind and name.
pub fn entry_kind(kind: &str, name: &str) -> Result<&'static str, String> {
    match (kind, name) {
        ("tool", _) => Ok("tool"),
        ("hook", "compose_system") => Ok("compose_system"),
        ("hook", "select_lessons") => Ok("select_lessons"),
        ("hook", other) => Err(format!(
            "unknown hook `{other}`: hooks must be named compose_system or select_lessons"
        )),
        (k, _) => Err(format!("kind `{k}` is not compiled")),
    }
}

fn fnv1a(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

impl Userspace {
    pub fn new(state_dir: &Path, target_dir: PathBuf) -> Userspace {
        Userspace { build_root: state_dir.join("build"), target_dir, loaded: HashMap::new(), last_build: None }
    }

    /// Builds (if not cached) and loads the library for one definition version.
    pub fn load(&mut self, name: &str, version: u32, kind: &str, source: &str) -> Result<Loaded, String> {
        let entry = entry_kind(kind, name)?;
        let hash = fnv1a(&[&WRAPPER_ABI.to_string(), entry, name, source]);
        let pkg = format!("cmh_us_{name}_{hash:016x}");
        let file = format!("{}{pkg}{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
        let dir = self.build_root.join(format!("{name}_v{version}"));
        let lib_path = dir.join(&file);
        if let Some(l) = self.loaded.get(&lib_path) {
            return Ok(*l);
        }
        self.last_build = None;
        if !lib_path.exists() {
            self.build(&dir, &pkg, &file, entry, source, &lib_path)?;
        }
        let loaded = open(&lib_path)?;
        self.loaded.insert(lib_path, loaded);
        Ok(loaded)
    }

    fn build(&mut self, dir: &Path, pkg: &str, file: &str, entry: &str, source: &str, lib_path: &Path) -> Result<(), String> {
        let io = |e: std::io::Error| format!("build dir {}: {e}", dir.display());
        std::fs::create_dir_all(dir.join("src")).map_err(io)?;
        std::fs::write(dir.join("Cargo.toml"), cargo_toml(pkg)).map_err(io)?;
        std::fs::write(dir.join("src/lib.rs"), wrapper(entry)).map_err(io)?;
        std::fs::write(dir.join("src/user.rs"), source).map_err(io)?;

        // Identical source compiled before (any state dir): reuse that library.
        let shared = self.target_dir.join("cmh-libs").join(file);
        if !shared.exists() {
            let started = Instant::now();
            let mut out = self.cargo(dir, true)?;
            if !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("offline") {
                // Dependency not in the local registry cache yet: fetch once.
                out = self.cargo(dir, false)?;
            }
            self.last_build = Some(started.elapsed());
            if !out.status.success() {
                return Err(trim_compiler_output(&String::from_utf8_lossy(&out.stderr)));
            }
            let built = self.target_dir.join("debug").join(file);
            atomic_copy(&built, &shared)?;
        }
        atomic_copy(&shared, lib_path)
    }

    fn cargo(&self, dir: &Path, offline: bool) -> Result<std::process::Output, String> {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.arg("build").arg("--manifest-path").arg(dir.join("Cargo.toml"));
        if offline {
            cmd.arg("--offline");
        }
        cmd.env("CARGO_TARGET_DIR", &self.target_dir)
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("CARGO_MANIFEST_DIR")
            .env("CARGO_TERM_COLOR", "never")
            .current_dir(dir)
            .output()
            .map_err(|e| format!("could not run cargo: {e}"))
    }
}

fn atomic_copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Copy to a fresh inode and rename over, so an already-mapped library is
    // never rewritten in place.
    let tmp = to.with_extension(format!("tmp{}", std::process::id()));
    std::fs::copy(from, &tmp).map_err(|e| format!("copy {} -> {}: {e}", from.display(), tmp.display()))?;
    std::fs::rename(&tmp, to).map_err(|e| format!("rename {}: {e}", to.display()))
}

fn open(path: &Path) -> Result<Loaded, String> {
    // SAFETY: the library is a kernel-generated wrapper whose initialisers do
    // nothing unusual. It is leaked and never unloaded.
    let lib = unsafe { Library::new(path) }.map_err(|e| format!("load {}: {e}", path.display()))?;
    let lib: &'static Library = Box::leak(Box::new(lib));
    // SAFETY: the symbols are exported by the generated wrapper with exactly
    // these signatures; the ABI marker guards against stale wrappers.
    unsafe {
        let abi: libloading::Symbol<unsafe extern "C" fn() -> u32> =
            lib.get(b"cmh_abi\0").map_err(|e| format!("missing cmh_abi: {e}"))?;
        if abi() != WRAPPER_ABI {
            return Err("stale userspace library (wrapper ABI mismatch)".into());
        }
        let call: libloading::Symbol<CallFn> = lib.get(b"cmh_call\0").map_err(|e| e.to_string())?;
        let free: libloading::Symbol<FreeFn> = lib.get(b"cmh_free\0").map_err(|e| e.to_string())?;
        Ok(Loaded { call: *call, free: *free })
    }
}

impl Loaded {
    /// Calls the entry point on a worker thread. A call that does not return
    /// within `timeout` is an error; the runaway thread is abandoned.
    pub fn call(&self, input: &Value, timeout: Duration) -> Result<String, String> {
        let arg = CString::new(input.to_string()).map_err(|e| e.to_string())?;
        let (call, free) = (self.call, self.free);
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("cmh-userspace".into())
            .stack_size(64 << 20)
            .spawn(move || {
                // SAFETY: `arg` is a valid NUL-terminated string that outlives
                // the call; the result is owned by the library and returned to
                // its own `cmh_free`.
                let out = unsafe {
                    let p = call(arg.as_ptr());
                    if p.is_null() {
                        Err("userspace returned null".to_string())
                    } else {
                        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                        free(p);
                        Ok(s)
                    }
                };
                let _ = tx.send(out);
            })
            .map_err(|e| format!("spawn worker: {e}"))?;
        let raw = match rx.recv_timeout(timeout) {
            Ok(r) => r?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!("timeout: no result after {:.1}s", timeout.as_secs_f64()));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("userspace worker died".into()),
        };
        let v: Value = serde_json::from_str(&raw).map_err(|e| format!("bad wrapper output: {e}"))?;
        match (v.get("ok").and_then(Value::as_str), v.get("err").and_then(Value::as_str)) {
            (Some(ok), _) => Ok(ok.to_string()),
            (_, Some(err)) => Err(err.to_string()),
            _ => Err(format!("bad wrapper output: {raw}")),
        }
    }
}

fn cargo_toml(pkg: &str) -> String {
    format!(
        r#"# Generated by the cmh kernel. Do not edit.
[package]
name = "{pkg}"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
path = "src/lib.rs"
crate-type = ["cdylib"]

[dependencies]
serde_json = "1"

[profile.dev]
debug = false
incremental = false

[workspace]
"#
    )
}

fn wrapper(entry: &str) -> String {
    let body = match entry {
        "tool" => "user::run(&input)",
        "compose_system" => {
            r#"let notes = strings(&input, "notes")?;
        let lessons = strings(&input, "lessons")?;
        let out: String = user::compose_system(&notes, &lessons);
        Ok(out)"#
        }
        _ => {
            r#"let task = input.get("task").and_then(|v| v.as_str()).ok_or("input.task must be a string")?.to_string();
        let lessons: Vec<serde_json::Value> =
            input.get("lessons").and_then(|v| v.as_array()).cloned().ok_or("input.lessons must be an array")?;
        let ids: Vec<i64> = user::select_lessons(&task, &lessons);
        serde_json::to_string(&ids).map_err(|e| e.to_string())"#
        }
    };
    format!(
        r#"// Generated by the cmh kernel. Do not edit.
#[allow(warnings)]
mod user;

use std::cell::RefCell;
use std::ffi::{{CStr, CString, c_char}};

thread_local! {{ static PANIC_AT: RefCell<String> = const {{ RefCell::new(String::new()) }}; }}

#[allow(dead_code)]
fn strings(input: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {{
    input.get(key).and_then(|v| v.as_array())
        .ok_or(format!("input.{{key}} must be an array of strings"))?
        .iter().map(|s| s.as_str().map(str::to_string).ok_or(format!("input.{{key}} must be an array of strings")))
        .collect()
}}

fn entry(raw: &str) -> Result<String, String> {{
    let input: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    {body}
}}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_abi() -> u32 {{ {WRAPPER_ABI} }}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_call(input: *const c_char) -> *mut c_char {{
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| std::panic::set_hook(Box::new(|info| {{
        let at = info.location().map(|l| format!(" at {{}}:{{}}", l.file(), l.line())).unwrap_or_default();
        PANIC_AT.with(|p| *p.borrow_mut() = at);
    }})));
    let raw = unsafe {{ CStr::from_ptr(input) }}.to_string_lossy().into_owned();
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&raw))) {{
        Ok(r) => r,
        Err(p) => {{
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".into());
            Err(format!("panic: {{msg}}{{}}", PANIC_AT.with(|p| p.borrow().clone())))
        }}
    }};
    let out = match result {{
        Ok(s) => serde_json::json!({{ "ok": s }}),
        Err(e) => serde_json::json!({{ "err": e }}),
    }};
    CString::new(out.to_string()).unwrap_or_default().into_raw()
}}

#[unsafe(no_mangle)]
pub extern "C" fn cmh_free(p: *mut c_char) {{
    if !p.is_null() {{ unsafe {{ drop(CString::from_raw(p)); }} }}
}}
"#
    )
}

/// Keeps the compiler's diagnostics, drops cargo progress noise, and caps size.
fn trim_compiler_output(stderr: &str) -> String {
    let noise = ["Compiling", "Updating", "Locking", "Downloading", "Downloaded", "Adding", "Blocking", "Finished"];
    let kept: Vec<&str> = stderr
        .lines()
        .filter(|l| !noise.contains(&l.trim_start().split(' ').next().unwrap_or("")))
        .filter(|l| !l.starts_with("error: could not compile"))
        .collect();
    let mut text = kept.join("\n").trim().to_string();
    if text.len() > 4000 {
        let mut cut = 4000;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n... (compiler output truncated)");
    }
    format!("compile error:\n{text}")
}
