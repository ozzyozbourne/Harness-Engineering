//! Kernel tools that touch the outside world: bash, read_file, write_file.

use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

pub const KERNEL_TOOLS: [&str; 10] = [
    "bash", "read_file", "write_file", "list_defs", "show_def", "forth_eval", "propose_def", "rollback_def", "remember", "recall",
];

pub fn schemas() -> Vec<Value> {
    let obj = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
    let t = |name: &str, desc: &str, schema: Value| json!({"name": name, "description": desc, "input_schema": schema});
    vec![
        t("bash", "Run a bash command in the working directory. Returns combined stdout/stderr and the exit code (truncated to 20 KB).",
          obj(json!({"command": {"type": "string"}, "timeout_s": {"type": "number", "description": "default 60"}}), &["command"])),
        t("read_file", "Read a file (truncated to 50 KB).", obj(json!({"path": {"type": "string"}}), &["path"])),
        t("write_file", "Write a file, creating parent directories. Refuses the state dir and the kernel source.",
          obj(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"])),
        t("list_defs", "List userspace definitions: name, kind, live version, status, description.", obj(json!({}), &[])),
        t("show_def", "Show one definition version (default: live) with source and tests.",
          obj(json!({"name": {"type": "string"}, "version": {"type": "integer"}}), &["name"])),
        t("forth_eval", concat!(
            "Scratchpad: compile and run cmh Forth in a throwaway WebAssembly VM, without proposing anything. ",
            "Use it to try code, inspect the stack or debug before propose_def; nothing is journaled, versioned or made live, ",
            "and live userspace words are callable. The source must define `: run ( json-addr json-len -- out-addr out-len ) ... ;` ",
            "exactly as for propose_def, so working code can be proposed unchanged. Returns the output string, or the same ",
            "structured error report the gate gives. Host words with side effects (file-write) still act on the working directory."),
          obj(json!({
              "source": {"type": "string"},
              "input": {"type": "object", "description": "JSON object passed to `run` (default {})"}
          }), &["source"])),
        t("propose_def", concat!(
            "Propose a new version of a userspace definition, written in cmh Forth (see the Forth reference in the system prompt). ",
            "It is compiled and tested in a fresh scratch WebAssembly VM, then goes live as a trial and is kept after clean uses or reverted on the first error.\n",
            "Every tool and hook source must define `: run ( json-addr json-len -- out-addr out-len ) ... ;` which receives the input as a JSON string and leaves the output string.\n",
            "- tool: input is the tool input object; needs input_schema.\n",
            "- hook `compose_system`: input {\"notes\": [string], \"lessons\": [string]}; output is the supplemental system prompt text.\n",
            "- hook `select_lessons`: input {\"task\": string, \"lessons\": [{\"id\", \"text\", \"score\"}]}; output is a JSON array of lesson ids, e.g. [3,1].\n",
            "- note: plain text appended to the system prompt.\n",
            "tests: [{\"input\": {...}, \"expect\": \"substring of output\"}], at least one for tools and hooks."),
          obj(json!({
              "name": {"type": "string", "description": "lowercase identifier [a-z][a-z0-9_]*"},
              "kind": {"type": "string", "enum": ["tool", "note", "hook"]},
              "source": {"type": "string"},
              "description": {"type": "string"},
              "tests": {"type": "array", "items": {"type": "object", "properties": {"input": {}, "expect": {"type": "string"}}}},
              "input_schema": {"type": "object", "description": "JSON Schema for a tool's input"}
          }), &["name", "kind", "source", "description"])),
        t("rollback_def", "Reinstall an older version of a definition as a new trial version.",
          obj(json!({"name": {"type": "string"}, "version": {"type": "integer"}}), &["name", "version"])),
        t("remember", "Record a durable, reusable lesson.", obj(json!({"text": {"type": "string"}}), &["text"])),
        t("recall", "Find lessons and verdicts sharing words with the query, highest score first.",
          obj(json!({"query": {"type": "string"}}), &["query"])),
    ]
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... (truncated, {} bytes total)", &s[..cut], s.len())
}

/// At most `max` characters of `s`, with "..." if anything was cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}...", s.chars().take(max).collect::<String>()) }
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input.get(key).and_then(Value::as_str).ok_or(format!("`{key}` (string) is required"))
}

pub fn bash(work_dir: &Path, input: &Value) -> Result<String, String> {
    let cmd = str_arg(input, "command")?;
    let timeout = input.get("timeout_s").and_then(Value::as_f64).unwrap_or(60.0).clamp(1.0, 3600.0);
    let (mut reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let writer2 = writer.try_clone().map_err(|e| e.to_string())?;
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer2)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn bash: {e}"))?;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
        }
        let _ = done_tx.send(());
    });
    let kill_group = |pid: u32| {
        let _ = Command::new("kill").args(["-9", "--", &format!("-{pid}")]).stderr(Stdio::null()).status();
    };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    // Background children may hold the pipe open; do not wait for them forever.
    if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
        kill_group(child.id());
        let _ = done_rx.recv_timeout(Duration::from_secs(1));
    }
    let out = String::from_utf8_lossy(&buf.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    let out = truncate(&out, 20 * 1024);
    match status {
        Some(st) if st.success() => Ok(format!("{out}\n[exit code: 0]")),
        Some(st) => Err(format!("{out}\n[exit code: {}]", st.code().map_or("signal".into(), |c| c.to_string()))),
        None => Err(format!("{out}\n[timed out after {timeout}s]")),
    }
}

/// Absolute, lexically normalised path with the existing prefix canonicalised.
pub fn resolve(work_dir: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { work_dir.join(p) };
    let mut norm = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            other => norm.push(other.as_os_str()),
        }
    }
    let mut existing = norm.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return norm,
        }
    }
    let mut out = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
    for part in rest.iter().rev() {
        out.push(part);
    }
    out
}

pub fn read_file(work_dir: &Path, input: &Value) -> Result<String, String> {
    let path = resolve(work_dir, str_arg(input, "path")?);
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(truncate(&String::from_utf8_lossy(&bytes), 50 * 1024))
}

pub fn write_file(work_dir: &Path, input: &Value, protected: &[PathBuf]) -> Result<String, String> {
    let path = resolve(work_dir, str_arg(input, "path")?);
    let content = str_arg(input, "content")?;
    if let Some(p) = protected_by(&path, protected) {
        return Err(format!("refused: {} is inside protected path {}", path.display(), p.display()));
    }
    write_creating_dirs(&path, content.as_bytes())?;
    Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
}

/// The protected path containing `path`, if any. Both must be resolved
/// (`resolve`) for the prefix test to mean anything.
pub fn protected_by<'a>(path: &Path, protected: &'a [PathBuf]) -> Option<&'a PathBuf> {
    protected.iter().find(|p| path.starts_with(p))
}

/// Writes a file, creating its parent directories (`write_file` and the
/// `file-write` host word).
pub fn write_creating_dirs(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
}
