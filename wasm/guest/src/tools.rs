//! Kernel tool schemas and the tools that touch the outside world (through
//! `Sys`): bash, read_file, write_file.

use crate::sys::Sys;
use crate::util::{normalize, truncate};
use serde_json::{Value, json};

pub const KERNEL_TOOLS: [&str; 12] = [
    "bash",
    "read_file",
    "write_file",
    "list_defs",
    "show_def",
    "forth_eval",
    "propose_def",
    "rollback_def",
    "remember",
    "recall",
    "propose_kernel",
    "list_images",
];

pub fn schemas() -> Vec<Value> {
    let obj = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
    let t = |name: &str, desc: &str, schema: Value| json!({"name": name, "description": desc, "input_schema": schema});
    vec![
        t(
            "bash",
            "Run a bash command in the working directory. Returns combined stdout/stderr and the exit code (truncated to 20 KB). The state dir, the kernel source and base-prompt.md are read-only inside it.",
            obj(json!({"command": {"type": "string"}, "timeout_s": {"type": "number", "description": "default 60"}}), &["command"]),
        ),
        t(
            "read_file",
            "Read a file (truncated to 50 KB): inside the working directory, the state dir (including kernel-src/) or the kernel source.",
            obj(json!({"path": {"type": "string"}}), &["path"]),
        ),
        t(
            "write_file",
            "Write a file, creating parent directories. Refuses the state dir, the kernel source and base-prompt.md.",
            obj(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"]),
        ),
        t("list_defs", "List userspace definitions: name, kind, live version, status, description.", obj(json!({}), &[])),
        t(
            "show_def",
            "Show one definition version (default: live) with source and tests.",
            obj(json!({"name": {"type": "string"}, "version": {"type": "integer"}}), &["name"]),
        ),
        t(
            "forth_eval",
            concat!(
                "Scratchpad: compile and run cmh Forth in a throwaway VM, without proposing anything. ",
                "Use it to try code, inspect the stack or debug before propose_def; nothing is journaled, versioned or made live, ",
                "and live userspace words are callable. The source must define `: run ( json-addr json-len -- out-addr out-len ) ... ;` ",
                "exactly as for propose_def, so working code can be proposed unchanged. Returns the output string, or the same ",
                "structured error report the gate gives. Host words with side effects (file-write) still act on the working directory."
            ),
            obj(
                json!({
                    "source": {"type": "string"},
                    "input": {"type": "object", "description": "JSON object passed to `run` (default {})"}
                }),
                &["source"],
            ),
        ),
        t(
            "propose_def",
            concat!(
                "Propose a new version of a userspace definition, written in cmh Forth (see the Forth reference in the system prompt). ",
                "It is compiled and tested in a fresh scratch VM, then goes live as a trial and is kept after clean uses or reverted on the first error.\n",
                "Every tool and hook source must define `: run ( json-addr json-len -- out-addr out-len ) ... ;` which receives the input as a JSON string and leaves the output string.\n",
                "- tool: input is the tool input object; needs input_schema.\n",
                "- hook `compose_system`: input {\"notes\": [string], \"lessons\": [string]}; output is the supplemental system prompt text.\n",
                "- hook `select_lessons`: input {\"task\": string, \"lessons\": [{\"id\", \"text\", \"score\"}]}; output is a JSON array of lesson ids, e.g. [3,1].\n",
                "- note: plain text appended to the system prompt.\n",
                "tests: [{\"input\": {...}, \"expect\": \"substring of output\"}], at least one for tools and hooks."
            ),
            obj(
                json!({
                    "name": {"type": "string", "description": "lowercase identifier [a-z][a-z0-9_]*"},
                    "kind": {"type": "string", "enum": ["tool", "note", "hook"]},
                    "source": {"type": "string"},
                    "description": {"type": "string"},
                    "tests": {"type": "array", "items": {"type": "object", "properties": {"input": {}, "expect": {"type": "string"}}}},
                    "input_schema": {"type": "object", "description": "JSON Schema for a tool's input"}
                }),
                &["name", "kind", "source", "description"],
            ),
        ),
        t(
            "rollback_def",
            "Reinstall an older version of a definition as a new trial version.",
            obj(json!({"name": {"type": "string"}, "version": {"type": "integer"}}), &["name", "version"]),
        ),
        t("remember", "Record a durable, reusable lesson.", obj(json!({"text": {"type": "string"}}), &["text"])),
        t(
            "recall",
            "Find lessons and verdicts sharing words with the query, highest score first.",
            obj(json!({"query": {"type": "string"}}), &["query"]),
        ),
        t(
            "propose_kernel",
            concat!(
                "Change your own kernel (this harness: agent loop, prompts, kernel tools, gate) by proposing a new kernel image. ",
                "The current kernel's Rust source is in the state dir under kernel-src/ (read it with read_file or bash first). ",
                "Give `edits` (exact string replacements, each old_str must occur exactly once) and/or `files` (whole new contents), ",
                "with paths relative to kernel-src/, under src/ or seed/ only (Cargo.toml, Cargo.lock and wit/ are fixed). ",
                "The hypervisor compiles the image for wasm32-wasip2, boots it in a scratch instance, runs its self-test and a ",
                "conformance scenario, and returns the verdict with the compiler or test output. An accepted image is a trial for ",
                "the next tasks (starting with the next one); it is kept after enough clean tasks and reverted if it traps, fails to boot ",
                "or a task fails. Builds take a while: try the change carefully, keep it small."
            ),
            obj(
                json!({
                    "description": {"type": "string", "description": "what the change does, one line"},
                    "edits": {"type": "array", "items": {"type": "object", "properties": {
                        "path": {"type": "string"}, "old_str": {"type": "string"}, "new_str": {"type": "string"}},
                        "required": ["path", "old_str", "new_str"]}},
                    "files": {"type": "object", "description": "path -> full new file content", "additionalProperties": {"type": "string"}}
                }),
                &["description"],
            ),
        ),
        t(
            "list_images",
            "List kernel image versions with status (builtin, trial, kept, reverted, rejected) and reasons.",
            obj(json!({}), &[]),
        ),
    ]
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input.get(key).and_then(Value::as_str).ok_or(format!("`{key}` (string) is required"))
}

pub fn bash(sys: &dyn Sys, input: &Value) -> Result<String, String> {
    let cmd = str_arg(input, "command")?;
    let timeout = input.get("timeout_s").and_then(Value::as_f64).unwrap_or(60.0).clamp(1.0, 3600.0) as u32;
    let o = sys.bash(cmd, timeout)?;
    match (o.timed_out, o.exit_code) {
        (true, _) => Err(format!("{}\n[timed out after {timeout}s]", o.output)),
        (false, 0) => Ok(format!("{}\n[exit code: 0]", o.output)),
        (false, -1) => Err(format!("{}\n[exit code: signal]", o.output)),
        (false, c) => Err(format!("{}\n[exit code: {c}]", o.output)),
    }
}

pub fn read_file(sys: &dyn Sys, work_dir: &str, input: &Value) -> Result<String, String> {
    let path = normalize(work_dir, str_arg(input, "path")?);
    let bytes = sys.read_file(&path)?;
    Ok(truncate(&String::from_utf8_lossy(&bytes), 50 * 1024))
}

pub fn write_file(sys: &dyn Sys, work_dir: &str, input: &Value) -> Result<String, String> {
    let path = normalize(work_dir, str_arg(input, "path")?);
    sys.write_file(&path, str_arg(input, "content")?)
}
