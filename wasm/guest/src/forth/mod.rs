//! The kernel's side of the Forth userspace: compiling definitions, calling
//! them with lazy loading, and turning the VM's JSON reports into the text
//! the model sees.

pub mod vm;

use crate::util::{clip, i64_field, str_field};
use serde_json::Value;
use std::collections::BTreeMap;
use vm::Status;
pub use vm::{ForthIo, Snapshot, Vm};

/// Steps (Forth instructions) per entry into a VM.
pub const MAX_STEPS: u64 = 20_000_000;

/// Live userspace code the VM may compile lazily: name -> (version, source).
pub type Live<'a> = BTreeMap<&'a str, (u32, &'a str)>;

pub struct CallOutcome {
    pub result: Result<String, String>,
    /// Definitions compiled lazily during the call.
    pub loads: Vec<(String, u32)>,
    /// Lessons added through the `remember` host word.
    pub lessons: Vec<String>,
}

pub struct ForthVm {
    pub vm: Vm,
}

impl ForthVm {
    pub fn new(io: Box<dyn ForthIo>) -> ForthVm {
        ForthVm { vm: Vm::new(io, MAX_STEPS) }
    }

    fn set_known(&mut self, live: &Live) {
        self.vm.known = live.keys().map(|k| k.to_string()).collect();
    }

    /// Compiles `source` as `name` version `version` and binds the public
    /// slot `name` to its `run`. Errors are formatted reports.
    pub fn define(&mut self, name: &str, version: u32, source: &str, live: &Live) -> Result<(), String> {
        self.set_known(live);
        self.vm.define(name, version, source).map_err(|r| format_report(r.as_bytes(), b""))
    }

    /// Calls the `run` of definition `name` with `input` (JSON text).
    /// Unbound definitions found in `live` are compiled on first use.
    pub fn call(&mut self, name: &str, input: &[u8], live: &Live) -> CallOutcome {
        self.set_known(live);
        let mut loads = Vec::new();
        let mut st = self.vm.call(name, input);
        let result = loop {
            match st {
                Status::Ok(out) => break Ok(String::from_utf8_lossy(&out).into_owned()),
                Status::Err(report) => break Err(format_report(report.as_bytes(), input)),
                // The VM hit an unbound public slot: compile that definition
                // (if it is live) and let the VM continue or fail.
                Status::Need(need) => {
                    let ok = match live.get(need.as_str()) {
                        Some((v, src)) => {
                            if let Err(rep) = self.vm.define(&need, *v, src) {
                                break Err(format!("lazy load of `{need}` v{v} failed:\n{}", format_report(rep.as_bytes(), b"")));
                            }
                            loads.push((need.clone(), *v));
                            true
                        }
                        None => false,
                    };
                    st = self.vm.resume(ok);
                }
            }
        };
        CallOutcome { result, loads, lessons: std::mem::take(&mut self.vm.lessons) }
    }
}

fn hint(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "stack-underflow" => "check each word's ( before -- after ) effect against what is really on the stack at that point",
        "stack-overflow" => "a loop pushes more than it drops; every iteration must leave the stack as it found it",
        "undefined-word" => "define helper words before `run` in the same source, or call a live userspace definition by its name",
        "fuel-exhausted" | "timeout" => {
            "a loop never terminates: make sure BEGIN ... UNTIL / WHILE conditions eventually change and that /string or counters advance"
        }
        "bad-address" => "use only addresses from HERE, VARIABLE, CREATE and strings; check that ( addr len ) pairs are not swapped",
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
        out.push_str(&format!("\n  output before the error: {:?}", clip(&output, 300)));
    }
    if !input.is_empty() {
        out.push_str(&format!("\n  input: {}", clip(&String::from_utf8_lossy(input), 300)));
    }
    if let Some(h) = hint(&kind) {
        out.push_str(&format!("\n  hint: {h}"));
    }
    out
}
