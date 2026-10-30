//! The kernel's view of the outside world. `HostSys` is the real thing: the
//! WIT imports from the hypervisor. `MemSys` is an in-memory world with a
//! scripted model, used by the image's own self-test (no side effects).

use crate::cmh::harness::host;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

pub struct BashOut {
    pub output: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

pub trait Sys {
    fn log(&self, line: &str);
    fn print(&self, text: &str);
    fn now_iso(&self) -> String;
    fn config(&self) -> Value;
    /// One model request; returns the response JSON.
    fn complete(&self, body: &Value) -> Result<Value, String>;
    fn bash(&self, command: &str, timeout_s: u32) -> Result<BashOut, String>;
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String>;
    fn write_file(&self, path: &str, content: &str) -> Result<String, String>;
    /// Appends an event and returns it as stored (with `ts`).
    fn journal_append(&self, ev: &Value) -> Result<Value, String>;
    fn journal_read(&self) -> Vec<Value>;
    fn checkpoint(&self, state: &Value);
    fn propose_image(&self, request: &Value) -> Result<String, String>;
    fn images(&self) -> String;
}

/// The hypervisor's imports.
pub struct HostSys;

impl Sys for HostSys {
    fn log(&self, line: &str) {
        host::log(line);
    }
    fn print(&self, text: &str) {
        host::print(text);
    }
    fn now_iso(&self) -> String {
        host::now_iso()
    }
    fn config(&self) -> Value {
        serde_json::from_str(&host::config()).unwrap_or(json!({}))
    }
    fn complete(&self, body: &Value) -> Result<Value, String> {
        let resp = host::post_messages(&body.to_string())?;
        serde_json::from_str(&resp).map_err(|e| format!("bad model response JSON: {e}"))
    }
    fn bash(&self, command: &str, timeout_s: u32) -> Result<BashOut, String> {
        host::bash(command, timeout_s).map(|o| BashOut { output: o.output, exit_code: o.exit_code, timed_out: o.timed_out })
    }
    /// Through the WASI read-only preopens (work dir, state dir, kernel source).
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| {
            format!("read {path}: {e} (read_file sees the work dir, the state dir and the kernel source; use bash for anything else)")
        })
    }
    fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        host::write_file(path, content)
    }
    fn journal_append(&self, ev: &Value) -> Result<Value, String> {
        let stored = host::journal_append(&ev.to_string())?;
        serde_json::from_str(&stored).map_err(|e| format!("bad journal reply: {e}"))
    }
    fn journal_read(&self) -> Vec<Value> {
        host::journal_read().iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
    fn checkpoint(&self, state: &Value) {
        host::checkpoint(&state.to_string());
    }
    fn propose_image(&self, request: &Value) -> Result<String, String> {
        host::propose_image(&request.to_string())
    }
    fn images(&self) -> String {
        host::images()
    }
}

pub const SELFTEST_BASE: &str = "SELF-TEST BASE PROMPT: you are cmh.";

/// An in-memory world for self-tests: journal, files and a scripted model
/// that records every request. Nothing leaves the image.
#[derive(Default)]
pub struct MemSys {
    pub journal: RefCell<Vec<Value>>,
    pub script: RefCell<VecDeque<Value>>,
    pub requests: RefCell<Vec<Value>>,
    pub files: RefCell<BTreeMap<String, Vec<u8>>>,
    pub logs: RefCell<Vec<String>>,
}

impl MemSys {
    pub fn with_script(script: Vec<Value>) -> MemSys {
        let m = MemSys::default();
        *m.script.borrow_mut() = script.into();
        m
    }
}

impl Sys for MemSys {
    fn log(&self, line: &str) {
        self.logs.borrow_mut().push(line.to_string());
    }
    fn print(&self, text: &str) {
        self.logs.borrow_mut().push(text.to_string());
    }
    fn now_iso(&self) -> String {
        "2026-01-01T00:00:00.000Z".into()
    }
    fn config(&self) -> Value {
        json!({"state-dir": "/selftest/.cmh", "work-dir": "/selftest", "kernel-dir": "/kernel", "soak": 3,
            "reflect": false, "max-turns": 10, "base-prompt": SELFTEST_BASE, "image": 0, "echo-text": false})
    }
    fn complete(&self, body: &Value) -> Result<Value, String> {
        self.requests.borrow_mut().push(body.clone());
        let Some(content) = self.script.borrow_mut().pop_front() else {
            return Err("self-test script exhausted".into());
        };
        let uses_tool = content.as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use"));
        Ok(json!({"content": content, "stop_reason": if uses_tool { "tool_use" } else { "end_turn" },
            "usage": {"input_tokens": 0, "output_tokens": 0}}))
    }
    fn bash(&self, _command: &str, _timeout_s: u32) -> Result<BashOut, String> {
        Err("bash is not available in the self-test".into())
    }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        self.files.borrow().get(path).cloned().ok_or(format!("read {path}: not found"))
    }
    fn write_file(&self, path: &str, content: &str) -> Result<String, String> {
        self.files.borrow_mut().insert(path.to_string(), content.as_bytes().to_vec());
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
    fn journal_append(&self, ev: &Value) -> Result<Value, String> {
        let mut ev = ev.clone();
        ev["ts"] = json!(self.now_iso());
        self.journal.borrow_mut().push(ev.clone());
        Ok(ev)
    }
    fn journal_read(&self) -> Vec<Value> {
        self.journal.borrow().clone()
    }
    fn checkpoint(&self, _state: &Value) {}
    fn propose_image(&self, _request: &Value) -> Result<String, String> {
        Err("propose_kernel is not available in the self-test".into())
    }
    fn images(&self) -> String {
        "(no images in the self-test)".into()
    }
}
