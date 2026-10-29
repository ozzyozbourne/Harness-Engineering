//! The 8 required tests from SPEC.md plus the Forth/WebAssembly wall tests,
//! all offline with the mock provider.

use cmh::kernel::provider::Mock;
use cmh::{Config, Kernel};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Value>>>;

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cmh-forth-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open(dir: &Path, script: Vec<Value>, soak: u32) -> (Kernel, Requests) {
    let mut cfg = Config::new(dir);
    cfg.soak = soak;
    cfg.reflect = false;
    cfg.echo_text = false;
    // Keep runaway loops in tests short.
    cfg.limits.fuel = 20_000_000;
    let mock = Mock::new(script);
    let reqs = mock.requests();
    (Kernel::open(cfg, Box::new(mock)).expect("open kernel"), reqs)
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!([{"type": "tool_use", "id": id, "name": name, "input": input}])
}

fn text(t: &str) -> Value {
    json!([{"type": "text", "text": t}])
}

fn status(k: &Kernel, name: &str) -> (Option<u32>, String) {
    let def = &k.state.defs[name];
    (def.live, def.live_version().map_or("none".into(), |v| v.status.clone()))
}

fn tool_def(name: &str, source: &str, input: Value, expect: &str) -> Value {
    json!({
        "name": name, "kind": "tool", "source": source, "description": "test version",
        "tests": [{"input": input, "expect": expect}],
        "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
    })
}

fn word_count_v2(source: &str, expect: &str) -> Value {
    tool_def("word_count", source, json!({"text": "a b"}), expect)
}

/// word_count v2 that tags its output with "v2:".
const V2_TAGGED: &str = r#"
: count ( a u -- n )  0 >r begin bl skip dup 0> while r> 1+ >r bl scan repeat 2drop r> ;
: skip ( a u c -- a' u' ) drop begin dup 0> if over c@ bl <= else false then while 1 /string repeat ;
: scan ( a u c -- a' u' ) drop begin dup 0> if over c@ bl >  else false then while 1 /string repeat ;
: run ( json-a json-u -- a u )  s" text" json-get count  <b s" v2:" b+ bn+ b> ;
"#;

const REVERSE: &str = r#"
\ {"text": string} -> the text reversed
: rev ( a u -- a2 u2 ) <b dup 0 ?do 2dup + i - 1- c@ bc+ loop 2drop b> ;
: run ( json-a json-u -- a u ) s" text" json-get rev ;
"#;

fn wc(k: &mut Kernel, t: &str) -> Result<String, String> {
    k.call_def("word_count", &json!({"text": t}), |_| Ok(()))
}

/// Last tool_result content in a recorded request.
fn last_tool_result(req: &Value) -> String {
    let msgs = req["messages"].as_array().unwrap();
    msgs.iter()
        .rev()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result")
        .map(|b| b["content"].as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

#[test]
fn seeds_pass_their_own_tests() {
    let dir = fresh_dir("seeds");
    let (mut k, _) = open(&dir, vec![], 3);
    for name in ["word_count", "compose_system", "select_lessons"] {
        let v = k.state.live(name).unwrap().clone();
        assert_eq!(v.status, "kept");
        for t in v.tests.as_array().unwrap() {
            let out = k.call_def(name, &t["input"], |_| Ok(())).unwrap();
            assert!(out.contains(t["expect"].as_str().unwrap()), "{name}: {out:?}");
        }
    }
    let out = k
        .call_def(
            "select_lessons",
            &json!({"task": "zzz", "lessons": [
                {"id": 1, "text": "a", "score": 1}, {"id": 2, "text": "b", "score": 7},
                {"id": 3, "text": "c", "score": 7}, {"id": 4, "text": "d", "score": -1}]}),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(out, "[2,3,1]", "fallback: top 3 by score, ties by id");
    assert_eq!(wc(&mut k, "  one two\tthree\n").unwrap(), "3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t1_late_binding() {
    let dir = fresh_dir("late");
    let script = vec![
        tool_use("a", "word_count", json!({"text": "x y z"})),
        tool_use("b", "propose_def", word_count_v2(V2_TAGGED, "v2:2")),
        tool_use("c", "word_count", json!({"text": "x y z"})),
        text("done"),
    ];
    let (mut k, reqs) = open(&dir, script, 3);
    let out = k.run_task("count words twice");
    assert!(out.success, "{:?}", out.error);
    let reqs = reqs.lock().unwrap();
    assert_eq!(last_tool_result(&reqs[1]), "3");
    assert!(last_tool_result(&reqs[2]).starts_with("trial v2"), "{}", last_tool_result(&reqs[2]));
    assert_eq!(last_tool_result(&reqs[3]), "v2:3");
    assert_eq!(status(&k, "word_count"), (Some(2), "trial".into()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t2_gate_rejection() {
    let dir = fresh_dir("reject");
    let (mut k, _) = open(&dir, vec![], 3);
    let err = k.propose_def(&word_count_v2(": run ( a u -- a u ) s\" x\" ", "2")).unwrap_err();
    assert!(err.contains("compile-error") && err.contains("missing `;`"), "{err}");
    let err = k.propose_def(&word_count_v2(": run 2drop s\" x\" frobnicate ;", "2")).unwrap_err();
    assert!(err.contains("undefined word `frobnicate`") && err.contains("line 1"), "{err}");
    let err = k.propose_def(&word_count_v2(": helper 1 ;", "2")).unwrap_err();
    assert!(err.contains("must define `: run"), "{err}");
    let err = k.propose_def(&word_count_v2(V2_TAGGED, "this is not in the output")).unwrap_err();
    assert!(err.contains("test 1 failed") && err.contains("\"v2:2\""), "{err}");
    let no_tests = json!({"name": "word_count", "kind": "tool", "source": V2_TAGGED, "description": "",
        "tests": [], "input_schema": {"type": "object"}});
    assert!(k.propose_def(&no_tests).is_err());
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    assert_eq!(k.vm_bound("word_count"), 1, "the live VM still runs v1");
    assert_eq!(wc(&mut k, "a b c").unwrap(), "3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t3_soak_revert() {
    let dir = fresh_dir("revert");
    let (mut k, _) = open(&dir, vec![], 3);
    // Divides by the text length: fine on tests, fails on empty text.
    let src = r#": run ( a u -- a u ) s" text" json-get nip 100 swap / n>s ;"#;
    k.propose_def(&word_count_v2(src, "33")).unwrap();
    assert_eq!(wc(&mut k, "a").unwrap(), "100");
    let err = wc(&mut k, "").unwrap_err();
    assert!(err.contains("[division-by-zero]") && err.contains("of word_count v2"), "{err}");
    assert!(err.contains("was reverted"), "{err}");
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    assert_eq!(k.state.defs["word_count"].get(2).unwrap().status, "reverted");
    assert_eq!(wc(&mut k, "a b").unwrap(), "2");
    let v = k.state.verdicts.iter().find(|v| v.name == "word_count" && v.version == 2).expect("verdict");
    assert_eq!(v.outcome, "reverted");
    assert!(v.text.contains("division-by-zero"), "{}", v.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t4_soak_keep() {
    let dir = fresh_dir("keep");
    let (mut k, _) = open(&dir, vec![], 3);
    k.propose_def(&word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    for i in 0..3 {
        assert_eq!(status(&k, "word_count"), (Some(2), "trial".into()), "before use {i}");
        wc(&mut k, "a b").unwrap();
    }
    assert_eq!(status(&k, "word_count"), (Some(2), "kept".into()));
    assert!(k.state.verdicts.iter().any(|v| v.version == 2 && v.outcome == "kept"));
    // Runtime errors from a kept version do not revert it.
    assert!(k.call_def("word_count", &json!({}), |_| Ok(())).is_err());
    assert_eq!(status(&k, "word_count"), (Some(2), "kept".into()));
    assert_eq!(wc(&mut k, "a b").unwrap(), "v2:2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t5_persistence() {
    let dir = fresh_dir("persist");
    let (mut k, _) = open(&dir, vec![], 3);
    k.propose_def(&word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    wc(&mut k, "a b").unwrap();
    k.remember("Prefer small userspace changes.", "remember").unwrap();
    k.propose_def(&json!({"name": "style", "kind": "note", "source": "Answer tersely.", "description": "style"})).unwrap();
    k.propose_def(&tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")).unwrap();
    let before = k.state.clone();
    assert_eq!(before.defs["word_count"].soak_uses, 1);
    drop(k);

    let (mut k2, _) = open(&dir, vec![], 3);
    assert_eq!(k2.state, before);
    assert_eq!(wc(&mut k2, "a b c").unwrap(), "v2:3");
    assert_eq!(k2.state.defs["word_count"].soak_uses, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t6_hook_redefinition() {
    let dir = fresh_dir("hook");
    let (mut k, reqs) = open(&dir, vec![text("first"), text("second")], 3);
    k.run_task("before");
    let src = r#"
: run ( json-a json-u -- a u )
  2dup s" notes" json-len >r  s" lessons" json-len >r
  <b s" CUSTOM-SUPPLEMENT notes=" b+ r> r> bn+ s"  lessons=" b+ bn+ b> ;"#;
    k.propose_def(&json!({"name": "compose_system", "kind": "hook", "source": src, "description": "custom",
        "tests": [{"input": {"notes": ["n"], "lessons": []}, "expect": "CUSTOM-SUPPLEMENT notes=1 lessons=0"}]}))
        .unwrap();
    k.run_task("after");
    let reqs = reqs.lock().unwrap();
    let system = |i: usize| -> Vec<String> {
        reqs[i]["system"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap().to_string()).collect()
    };
    assert!(!system(0).join("\n").contains("CUSTOM-SUPPLEMENT"));
    let after = system(1);
    assert!(after[0].starts_with("You are cmh"), "base prompt must stay first");
    assert!(after[1].contains("cmh Forth reference"), "the kernel's Forth reference follows it");
    assert!(after[2].contains("CUSTOM-SUPPLEMENT notes=0 lessons=0"), "{after:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t7_lesson_scoring() {
    let dir = fresh_dir("score");
    // One successful reply; later tasks exhaust the script and fail.
    let (mut k, reqs) = open(&dir, vec![text("ok")], 3);
    k.remember("Parser changes need regression tests.", "remember").unwrap();
    let score = |k: &Kernel| (k.state.lessons[0].score, k.state.lessons[0].retired);
    assert!(k.run_task("fix the parser").success);
    assert_eq!(score(&k), (1, false));
    let supp = reqs.lock().unwrap()[0]["system"][2]["text"].as_str().unwrap().to_string();
    assert!(supp.contains("## Lessons\n- Parser changes need"), "{supp}");
    for expected in [0, -1, -2] {
        assert!(!k.run_task("fix the parser").success);
        assert_eq!(score(&k), (expected, false));
    }
    assert!(!k.run_task("fix the parser").success);
    assert_eq!(score(&k), (-3, true));
    k.run_task("fix the parser");
    assert_eq!(score(&k), (-3, true));
    let _ = std::fs::remove_dir_all(&dir);
}

fn run_bin(state: &Path, script: &Value, task: &str) -> (i32, String, String) {
    let script_path = state.with_extension("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cmh"))
        .args(["--state", state.to_str().unwrap(), "--provider", "mock", "--script", script_path.to_str().unwrap()])
        .args(["--no-reflect", "run", task])
        .output()
        .unwrap();
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code().unwrap_or(-1), s(&out.stdout), s(&out.stderr))
}

#[test]
fn t8_end_to_end_run() {
    let dir = fresh_dir("e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let script = json!([
        tool_use("p", "propose_def", tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")),
        tool_use("r", "reverse", json!({"text": "hello"})),
        text("olleh"),
    ]);
    let state = dir.join("state");
    let (code, stdout, stderr) = run_bin(&state, &script, "reverse hello");
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "olleh\n", "stdout must be only the answer");
    assert!(stderr.contains("[tool] propose_def reverse -> trial v1"), "{stderr}");
    assert!(stderr.contains("[tool] reverse"), "{stderr}");
    let log = std::fs::read_to_string(state.join("mock-requests.jsonl")).unwrap();
    let reqs: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(reqs.len(), 3);
    let tools = |r: &Value| -> Vec<String> {
        r["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    assert!(!tools(&reqs[0]).contains(&"reverse".into()));
    assert!(tools(&reqs[1]).contains(&"reverse".into()));
    assert_eq!(last_tool_result(&reqs[2]), "olleh");

    // A failing run exits nonzero (script exhausted) and prints nothing.
    let (code, stdout, _) = run_bin(&state, &json!([]), "anything");
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------ Forth-specific

#[test]
fn forth_late_binding_existing_caller() {
    let dir = fresh_dir("caller");
    let (mut k, _) = open(&dir, vec![], 3);
    // `tagged` calls word_count through its slot; it is never recompiled.
    let caller = r#": run ( json-a json-u -- a u ) word_count s"  words" concat ;"#;
    k.propose_def(&tool_def("tagged", caller, json!({"text": "a b"}), "2 words")).unwrap();
    let call = |k: &mut Kernel| k.call_def("tagged", &json!({"text": "x y z"}), |_| Ok(())).unwrap();
    assert_eq!(call(&mut k), "3 words");
    k.propose_def(&word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    assert_eq!(call(&mut k), "v2:3 words", "the existing caller uses the new word_count");
    // Revert word_count v2 (bad input), and the caller follows the slot back to v1.
    assert!(wc(&mut k, "").is_ok());
    let src = r#": run ( a u -- a u ) s" text" json-get nip 0= if 1 0 / then s" v3" ;"#;
    k.propose_def(&word_count_v2(src, "v3")).unwrap();
    assert!(wc(&mut k, "").is_err());
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    assert_eq!(call(&mut k), "3 words");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forth_infinite_loop_stopped_by_fuel() {
    let dir = fresh_dir("fuel");
    let (mut k, _) = open(&dir, vec![], 3);
    // Loops forever when the text is "spin".
    let src = r#"
: spin ( -- ) begin false until ;
: run ( a u -- a u ) s" text" json-get 2dup s" spin" s= if spin then nip n>s ;"#;
    k.propose_def(&word_count_v2(src, "3")).unwrap();
    assert_eq!(wc(&mut k, "abcd").unwrap(), "4");
    let t0 = std::time::Instant::now();
    let err = wc(&mut k, "spin").unwrap_err();
    assert!(t0.elapsed() < std::time::Duration::from_secs(10));
    assert!(err.contains("[fuel-exhausted]") && err.contains("in word `spin` of word_count v2"), "{err}");
    assert!(err.contains("called from: `run`"), "{err}");
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    assert!(k.state.verdicts.iter().any(|v| v.version == 2 && v.text.contains("fuel-exhausted")));
    // The host and the VM keep working.
    assert_eq!(wc(&mut k, "a b").unwrap(), "2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forth_out_of_bounds_trap_restores_snapshot() {
    let dir = fresh_dir("oob");
    let (mut k, _) = open(&dir, vec![], 3);
    k.propose_def(&tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")).unwrap();
    for _ in 0..3 {
        k.call_def("reverse", &json!({"text": "abc"}), |_| Ok(())).unwrap();
    }
    assert_eq!(status(&k, "reverse"), (Some(1), "kept".into()));
    // Scribbles over data space, then stores far past the end of linear memory.
    let src = r#"
: scribble ( -- ) here 1000 0 do 255 over i + c! loop drop ;
: run ( a u -- a u )
  s" text" json-get 2dup s" boom" s= if scribble 7 4000000000 ! then nip n>s ;"#;
    k.propose_def(&word_count_v2(src, "3")).unwrap();
    let stats = k.vm_stats();
    let err = wc(&mut k, "boom").unwrap_err();
    assert!(err.contains("[wasm-out-of-bounds]") && err.contains("of word_count v2") && err.contains("at `!`"), "{err}");
    assert!(err.contains(" 7 4000000000]"), "the stack at the trap: {err}");
    assert_eq!(k.vm_stats(), stats, "the snapshot was restored");
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    // The dictionary still works: the seed, the kept tool, and new definitions.
    assert_eq!(wc(&mut k, "a b c").unwrap(), "3");
    assert_eq!(k.call_def("reverse", &json!({"text": "xyz"}), |_| Ok(())).unwrap(), "zyx");
    k.propose_def(&tool_def("tagged", r#": run word_count s" !" concat ;"#, json!({"text": "a"}), "1!")).unwrap();
    assert_eq!(k.call_def("tagged", &json!({"text": "p q"}), |_| Ok(())).unwrap(), "2!");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forth_stack_underflow_is_structured() {
    let dir = fresh_dir("underflow");
    let (mut k, _) = open(&dir, vec![], 3);
    let src = r#"
: combine ( a b -- a+b ) + ;
: run ( a u -- a u ) s" text" json-get nip  dup 3 > if drop combine then n>s ;"#;
    k.propose_def(&word_count_v2(src, "3")).unwrap();
    let err = wc(&mut k, "abcdef").unwrap_err();
    assert!(err.starts_with("forth error [stack-underflow] in word `combine` of word_count v2, line 2, at `+`"), "{err}");
    assert!(err.contains("needs 2 item(s) but the stack has 0"), "{err}");
    assert!(err.contains("called from: `run` (word_count v2) line 3 at `combine`"), "{err}");
    assert!(err.contains("input: {\"text\":\"abcdef\"}"), "{err}");
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forth_lazy_load_in_fresh_process() {
    let dir = fresh_dir("lazy");
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("state");
    let script = json!([
        tool_use("p", "propose_def", tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")),
        text("proposed"),
    ]);
    let (code, _, stderr) = run_bin(&state, &script, "make reverse");
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("lazy-load"), "a new proposal is compiled eagerly: {stderr}");

    // A fresh process compiles only the seeds; reverse loads on first call.
    let script = json!([tool_use("r", "reverse", json!({"text": "stressed"})), text("desserts")]);
    let (code, stdout, stderr) = run_bin(&state, &script, "use reverse");
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "desserts\n");
    assert!(stderr.contains("[forth] lazy-load reverse v1"), "{stderr}");
    assert!(stderr.contains("[tool] reverse {\"text\":\"stressed\"} -> desserts"), "{stderr}");

    // Same thing in-process: a fresh kernel has it unbound until the first call.
    let (mut k, _) = open(&state, vec![], 3);
    assert_eq!(k.vm_bound("reverse"), 0);
    assert_eq!(k.vm_bound("word_count"), 1);
    assert_eq!(k.call_def("reverse", &json!({"text": "ab"}), |_| Ok(())).unwrap(), "ba");
    assert_eq!(k.lazy_loads, vec![("reverse".to_string(), 1)]);
    assert_eq!(k.vm_bound("reverse"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forth_eval_scratchpad_runs_code_without_proposing() {
    let dir = fresh_dir("eval");
    let script = vec![
        tool_use("e1", "forth_eval", json!({"source": ": run ( a u -- a u ) 2drop + s\" x\" ;"})),
        tool_use("e2", "forth_eval", json!({"source": REVERSE, "input": {"text": "abc"}})),
        tool_use("e3", "forth_eval", json!({"source": ": run word_count ;", "input": {"text": "one two three"}})),
        text("done"),
    ];
    let (mut k, reqs) = open(&dir, script, 3);
    assert!(k.run_task("try some forth").success);
    let reqs = reqs.lock().unwrap();
    assert_eq!(last_tool_result(&reqs[2]), "output: cba");
    let err = last_tool_result(&reqs[1]);
    assert!(err.contains("stack-underflow") && err.contains("`+`"), "{err}");
    // Live userspace words are callable through their late-bound slots.
    assert_eq!(last_tool_result(&reqs[3]), "output: 3");
    // Nothing was proposed or registered.
    assert!(!k.state.defs.contains_key("scratchpad"));
    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    assert!(!journal.contains("def_proposed") && !journal.contains("def_rejected"));
    let _ = std::fs::remove_dir_all(&dir);
}
