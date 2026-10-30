//! The 8 required tests from SPEC.md plus the Forth tests that still apply,
//! all offline with the mock provider. Everything runs through the kernel
//! image (the WebAssembly component) under the hypervisor.

mod common;

use common::*;
use serde_json::json;

#[test]
fn seeds_pass_their_own_tests() {
    let dir = TestDir::new("seeds");
    let (mut hv, _) = open(&dir, vec![], 3);
    let s = state(&mut hv);
    for name in ["word_count", "compose_system", "select_lessons"] {
        assert_eq!(status(&mut hv, name), (Some(1), "kept".into()));
        for t in s["defs"][name]["versions"][0]["tests"].as_array().unwrap() {
            let out = call(&mut hv, name, t["input"].clone()).unwrap();
            assert!(out.contains(t["expect"].as_str().unwrap()), "{name}: {out:?}");
        }
    }
    let out = call(
        &mut hv,
        "select_lessons",
        json!({"task": "zzz", "lessons": [
            {"id": 1, "text": "a", "score": 1}, {"id": 2, "text": "b", "score": 7},
            {"id": 3, "text": "c", "score": 7}, {"id": 4, "text": "d", "score": -1}]}),
    )
    .unwrap();
    assert_eq!(out, "[2,3,1]", "fallback: top 3 by score, ties by id");
    assert_eq!(wc(&mut hv, "  one two\tthree\n").unwrap(), "3");
}

#[test]
fn builtin_image_passes_its_own_self_test() {
    let dir = TestDir::new("selftest");
    let (mut hv, _) = open(&dir, vec![], 3);
    assert_eq!(hv.self_test().unwrap(), "4 self-test scenarios passed");
    assert!(journal(&dir).iter().all(|e| e["type"] != "task_started"), "the self-test leaves no trace in the journal");
}

#[test]
fn t1_late_binding() {
    let dir = TestDir::new("late");
    let script = vec![
        tool_use("a", "word_count", json!({"text": "x y z"})),
        tool_use("b", "propose_def", word_count_v2(V2_TAGGED, "v2:2")),
        tool_use("c", "word_count", json!({"text": "x y z"})),
        text("done"),
    ];
    let (mut hv, reqs) = open(&dir, script, 3);
    let out = hv.run_task("count words twice");
    assert!(out.success, "{:?}", out.error);
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(last_tool_result(&reqs[1]), "3");
    assert!(last_tool_result(&reqs[2]).starts_with("trial v2"), "{}", last_tool_result(&reqs[2]));
    assert_eq!(last_tool_result(&reqs[3]), "v2:3");
    assert_eq!(status(&mut hv, "word_count"), (Some(2), "trial".into()));
}

#[test]
fn t2_gate_rejection() {
    let dir = TestDir::new("reject");
    let (mut hv, _) = open(&dir, vec![], 3);
    let err = propose(&mut hv, &word_count_v2(": run ( a u -- a u ) s\" x\" ", "2")).unwrap_err();
    assert!(err.contains("compile-error") && err.contains("missing `;`"), "{err}");
    let err = propose(&mut hv, &word_count_v2(": run 2drop s\" x\" frobnicate ;", "2")).unwrap_err();
    assert!(err.contains("undefined word `frobnicate`") && err.contains("line 1"), "{err}");
    let err = propose(&mut hv, &word_count_v2(": helper 1 ;", "2")).unwrap_err();
    assert!(err.contains("must define `: run"), "{err}");
    let err = propose(&mut hv, &word_count_v2(V2_TAGGED, "this is not in the output")).unwrap_err();
    assert!(err.contains("test 1 failed") && err.contains("\"v2:2\""), "{err}");
    let no_tests = json!({"name": "word_count", "kind": "tool", "source": V2_TAGGED, "description": "",
        "tests": [], "input_schema": {"type": "object"}});
    assert!(propose(&mut hv, &no_tests).is_err());
    assert_eq!(status(&mut hv, "word_count"), (Some(1), "kept".into()));
    assert_eq!(hv.command("/bound word_count").unwrap(), "1", "the live VM still runs v1");
    assert_eq!(wc(&mut hv, "a b c").unwrap(), "3");
}

#[test]
fn t3_soak_revert() {
    let dir = TestDir::new("revert");
    let (mut hv, _) = open(&dir, vec![], 3);
    // Divides by the text length: fine on tests, fails on empty text.
    let src = r#": run ( a u -- a u ) s" text" json-get nip 100 swap / n>s ;"#;
    propose(&mut hv, &word_count_v2(src, "33")).unwrap();
    assert_eq!(wc(&mut hv, "a").unwrap(), "100");
    let err = wc(&mut hv, "").unwrap_err();
    assert!(err.contains("[division-by-zero]") && err.contains("of word_count v2"), "{err}");
    assert!(err.contains("was reverted"), "{err}");
    assert_eq!(status(&mut hv, "word_count"), (Some(1), "kept".into()));
    assert_eq!(state(&mut hv)["defs"]["word_count"]["versions"][1]["status"], "reverted");
    assert_eq!(wc(&mut hv, "a b").unwrap(), "2");
    let v = verdicts(&mut hv).into_iter().find(|v| v["name"] == "word_count" && v["version"] == 2).expect("verdict");
    assert_eq!(v["outcome"], "reverted");
    assert!(v["text"].as_str().unwrap().contains("division-by-zero"), "{v}");
}

#[test]
fn t4_soak_keep() {
    let dir = TestDir::new("keep");
    let (mut hv, _) = open(&dir, vec![], 3);
    propose(&mut hv, &word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    for i in 0..3 {
        assert_eq!(status(&mut hv, "word_count"), (Some(2), "trial".into()), "before use {i}");
        wc(&mut hv, "a b").unwrap();
    }
    assert_eq!(status(&mut hv, "word_count"), (Some(2), "kept".into()));
    assert!(verdicts(&mut hv).iter().any(|v| v["version"] == 2 && v["outcome"] == "kept"));
    // Runtime errors from a kept version do not revert it.
    assert!(call(&mut hv, "word_count", json!({})).is_err());
    assert_eq!(status(&mut hv, "word_count"), (Some(2), "kept".into()));
    assert_eq!(wc(&mut hv, "a b").unwrap(), "v2:2");
}

#[test]
fn t5_persistence() {
    let dir = TestDir::new("persist");
    let (mut hv, _) = open(&dir, vec![], 3);
    propose(&mut hv, &word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    wc(&mut hv, "a b").unwrap();
    hv.command("/remember Prefer small userspace changes.").unwrap();
    propose(&mut hv, &json!({"name": "style", "kind": "note", "source": "Answer tersely.", "description": "style"})).unwrap();
    propose(&mut hv, &tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")).unwrap();
    let before = state(&mut hv);
    assert_eq!(before["defs"]["word_count"]["soak_uses"], 1);
    drop(hv);

    let (mut hv2, _) = open(&dir, vec![], 3);
    assert_eq!(state(&mut hv2), before);
    assert_eq!(wc(&mut hv2, "a b c").unwrap(), "v2:3");
    assert_eq!(state(&mut hv2)["defs"]["word_count"]["soak_uses"], 2);
}

#[test]
fn t6_hook_redefinition() {
    let dir = TestDir::new("hook");
    let (mut hv, reqs) = open(&dir, vec![text("first"), text("second")], 3);
    hv.run_task("before");
    let src = r#"
: run ( json-a json-u -- a u )
  2dup s" notes" json-len >r  s" lessons" json-len >r
  <b s" CUSTOM-SUPPLEMENT notes=" b+ r> r> bn+ s"  lessons=" b+ bn+ b> ;"#;
    propose(
        &mut hv,
        &json!({"name": "compose_system", "kind": "hook", "source": src, "description": "custom",
        "tests": [{"input": {"notes": ["n"], "lessons": []}, "expect": "CUSTOM-SUPPLEMENT notes=1 lessons=0"}]}),
    )
    .unwrap();
    hv.run_task("after");
    let reqs = reqs.lock().unwrap();
    assert!(!system_texts(&reqs[0]).join("\n").contains("CUSTOM-SUPPLEMENT"));
    let after = system_texts(&reqs[1]);
    assert!(after[0].starts_with("You are cmh"), "base prompt must stay first");
    assert!(after[1].contains("cmh Forth reference"), "the kernel's Forth reference follows it");
    assert!(after.last().unwrap().contains("CUSTOM-SUPPLEMENT notes=0 lessons=0"), "{after:?}");
}

#[test]
fn t7_lesson_scoring() {
    let dir = TestDir::new("score");
    // One successful reply; later tasks exhaust the script and fail.
    let (mut hv, reqs) = open(&dir, vec![text("ok")], 3);
    hv.command("/remember Parser changes need regression tests.").unwrap();
    let score = |hv: &mut cmh::Hypervisor| {
        let s = state(hv);
        (s["lessons"][0]["score"].as_i64().unwrap(), s["lessons"][0]["retired"].as_bool().unwrap())
    };
    assert!(hv.run_task("fix the parser").success);
    assert_eq!(score(&mut hv), (1, false));
    let supp = system_texts(&reqs.lock().unwrap()[0]).last().unwrap().clone();
    assert!(supp.contains("## Lessons\n- Parser changes need"), "{supp}");
    for expected in [0, -1, -2] {
        assert!(!hv.run_task("fix the parser").success);
        assert_eq!(score(&mut hv), (expected, false));
    }
    assert!(!hv.run_task("fix the parser").success);
    assert_eq!(score(&mut hv), (-3, true));
    hv.run_task("fix the parser");
    assert_eq!(score(&mut hv), (-3, true));
}

#[test]
fn t8_end_to_end_run() {
    let dir = TestDir::new("e2e");
    let script = json!([
        tool_use("p", "propose_def", tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")),
        tool_use("r", "reverse", json!({"text": "hello"})),
        text("olleh"),
    ]);
    let (code, stdout, stderr) = run_bin(&dir, &script, &["run", "reverse hello"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "olleh\n", "stdout must be only the answer");
    assert!(stderr.contains("[tool] propose_def reverse -> trial v1"), "{stderr}");
    assert!(stderr.contains("[tool] reverse"), "{stderr}");
    let reqs = mock_log(&dir);
    assert_eq!(reqs.len(), 3);
    assert!(!tool_names(&reqs[0]).contains(&"reverse".into()));
    assert!(tool_names(&reqs[1]).contains(&"reverse".into()));
    assert_eq!(last_tool_result(&reqs[2]), "olleh");

    // A failing run exits nonzero (script exhausted) and prints nothing.
    let (code, stdout, _) = run_bin(&dir, &json!([]), &["run", "anything"]);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    // Usage errors exit 2.
    let (code, _, stderr) = run_bin(&dir, &json!([]), &["frobnicate"]);
    assert_eq!(code, 2, "{stderr}");
}

// ------------------------------------------------------ Forth-specific

#[test]
fn forth_late_binding_existing_caller() {
    let dir = TestDir::new("caller");
    let (mut hv, _) = open(&dir, vec![], 3);
    // `tagged` calls word_count through its slot; it is never recompiled.
    let caller = r#": run ( json-a json-u -- a u ) word_count s"  words" concat ;"#;
    propose(&mut hv, &tool_def("tagged", caller, json!({"text": "a b"}), "2 words")).unwrap();
    let tagged = |hv: &mut cmh::Hypervisor| call(hv, "tagged", json!({"text": "x y z"})).unwrap();
    assert_eq!(tagged(&mut hv), "3 words");
    propose(&mut hv, &word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    assert_eq!(tagged(&mut hv), "v2:3 words", "the existing caller uses the new word_count");
    assert!(wc(&mut hv, "").is_ok());
    let src = r#": run ( a u -- a u ) s" text" json-get nip 0= if 1 0 / then s" v3" ;"#;
    propose(&mut hv, &word_count_v2(src, "v3")).unwrap();
    assert!(wc(&mut hv, "").is_err());
    assert_eq!(status(&mut hv, "word_count"), (Some(1), "kept".into()));
    assert_eq!(tagged(&mut hv), "3 words");
}

#[test]
fn forth_infinite_loop_stopped_by_step_budget() {
    let dir = TestDir::new("fuel");
    let (mut hv, _) = open(&dir, vec![], 3);
    let src = r#"
: spin ( -- ) begin false until ;
: run ( a u -- a u ) s" text" json-get 2dup s" spin" s= if spin then nip n>s ;"#;
    propose(&mut hv, &word_count_v2(src, "3")).unwrap();
    assert_eq!(wc(&mut hv, "abcd").unwrap(), "4");
    let t0 = std::time::Instant::now();
    let err = wc(&mut hv, "spin").unwrap_err();
    assert!(t0.elapsed() < std::time::Duration::from_secs(10));
    assert!(err.contains("[fuel-exhausted]") && err.contains("in word `spin` of word_count v2"), "{err}");
    assert!(err.contains("called from: `run`"), "{err}");
    assert_eq!(status(&mut hv, "word_count"), (Some(1), "kept".into()));
    assert!(verdicts(&mut hv).iter().any(|v| v["version"] == 2 && v["text"].as_str().unwrap().contains("fuel-exhausted")));
    assert_eq!(wc(&mut hv, "a b").unwrap(), "2");
}

#[test]
fn forth_bad_address_is_refused_and_snapshot_restored() {
    let dir = TestDir::new("oob");
    let (mut hv, _) = open(&dir, vec![], 3);
    propose(&mut hv, &tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")).unwrap();
    for _ in 0..3 {
        call(&mut hv, "reverse", json!({"text": "abc"})).unwrap();
    }
    assert_eq!(status(&mut hv, "reverse"), (Some(1), "kept".into()));
    // Scribbles over data space, then stores far outside it.
    let src = r#"
: scribble ( -- ) here 1000 0 do 255 over i + c! loop drop ;
: run ( a u -- a u )
  s" text" json-get 2dup s" boom" s= if scribble 7 4000000000 ! then nip n>s ;"#;
    propose(&mut hv, &word_count_v2(src, "3")).unwrap();
    let stats = hv.command("/stats").unwrap();
    let err = wc(&mut hv, "boom").unwrap_err();
    assert!(err.contains("[bad-address]") && err.contains("of word_count v2") && err.contains("at `!`"), "{err}");
    assert!(err.contains(" 7 4000000000]"), "the stack at the fault: {err}");
    assert_eq!(hv.command("/stats").unwrap(), stats, "the snapshot was restored");
    assert_eq!(status(&mut hv, "word_count"), (Some(1), "kept".into()));
    assert_eq!(wc(&mut hv, "a b c").unwrap(), "3");
    assert_eq!(call(&mut hv, "reverse", json!({"text": "xyz"})).unwrap(), "zyx");
    propose(&mut hv, &tool_def("tagged", r#": run word_count s" !" concat ;"#, json!({"text": "a"}), "1!")).unwrap();
    assert_eq!(call(&mut hv, "tagged", json!({"text": "p q"})).unwrap(), "2!");
}

#[test]
fn forth_stack_underflow_is_structured() {
    let dir = TestDir::new("underflow");
    let (mut hv, _) = open(&dir, vec![], 3);
    let src = r#"
: combine ( a b -- a+b ) + ;
: run ( a u -- a u ) s" text" json-get nip  dup 3 > if drop combine then n>s ;"#;
    propose(&mut hv, &word_count_v2(src, "3")).unwrap();
    let err = wc(&mut hv, "abcdef").unwrap_err();
    assert!(err.starts_with("forth error [stack-underflow] in word `combine` of word_count v2, line 2, at `+`"), "{err}");
    assert!(err.contains("needs 2 item(s) but the stack has 0"), "{err}");
    assert!(err.contains("called from: `run` (word_count v2) line 3 at `combine`"), "{err}");
    assert!(err.contains("input: {\"text\":\"abcdef\"}"), "{err}");
    assert_eq!(status(&mut hv, "word_count"), (Some(1), "kept".into()));
}

#[test]
fn forth_lazy_load_in_fresh_process() {
    let dir = TestDir::new("lazy");
    let script = json!([tool_use("p", "propose_def", tool_def("reverse", REVERSE, json!({"text": "abc"}), "cba")), text("proposed"),]);
    let (code, _, stderr) = run_bin(&dir, &script, &["run", "make reverse"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("lazy-load"), "a new proposal is compiled eagerly: {stderr}");

    // A fresh process compiles only the seeds; reverse loads on first call.
    let script = json!([tool_use("r", "reverse", json!({"text": "stressed"})), text("desserts")]);
    let (code, stdout, stderr) = run_bin(&dir, &script, &["run", "use reverse"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "desserts\n");
    assert!(stderr.contains("[forth] lazy-load reverse v1"), "{stderr}");
    assert!(stderr.contains("[tool] reverse {\"text\":\"stressed\"} -> desserts"), "{stderr}");

    let (mut hv, _) = open(&dir, vec![], 3);
    assert_eq!(hv.command("/bound reverse").unwrap(), "0");
    assert_eq!(hv.command("/bound word_count").unwrap(), "1");
    assert_eq!(call(&mut hv, "reverse", json!({"text": "ab"})).unwrap(), "ba");
    assert_eq!(hv.command("/lazy").unwrap(), r#"[["reverse",1]]"#);
    assert_eq!(hv.command("/bound reverse").unwrap(), "1");
}

#[test]
fn forth_eval_scratchpad_runs_code_without_proposing() {
    let dir = TestDir::new("eval");
    let script = vec![
        tool_use("e1", "forth_eval", json!({"source": ": run ( a u -- a u ) 2drop + s\" x\" ;"})),
        tool_use("e2", "forth_eval", json!({"source": REVERSE, "input": {"text": "abc"}})),
        tool_use("e3", "forth_eval", json!({"source": ": run word_count ;", "input": {"text": "one two three"}})),
        text("done"),
    ];
    let (mut hv, reqs) = open(&dir, script, 3);
    assert!(hv.run_task("try some forth").success);
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(last_tool_result(&reqs[2]), "output: cba");
    let err = last_tool_result(&reqs[1]);
    assert!(err.contains("stack-underflow") && err.contains("`+`"), "{err}");
    assert_eq!(last_tool_result(&reqs[3]), "output: 3");
    assert!(state(&mut hv)["defs"].get("scratchpad").is_none());
    let journal = std::fs::read_to_string(dir.state().join("journal.jsonl")).unwrap();
    assert!(!journal.contains("def_proposed") && !journal.contains("def_rejected"));
}

// ------------------------------------------------------- hypervisor wall

#[test]
fn journal_schema_and_protected_paths_hold() {
    let dir = TestDir::new("wall");
    let script = vec![
        tool_use("w", "write_file", json!({"path": dir.state().join("journal.jsonl").to_str().unwrap(), "content": "x"})),
        tool_use("b", "bash", json!({"command": format!("echo bad >> {}/journal.jsonl", dir.state().display())})),
        tool_use("o", "write_file", json!({"path": "notes/ok.txt", "content": "fine"})),
        text("done"),
    ];
    let (mut hv, reqs) = open(&dir, script, 3);
    hv.run_task("try to break the rules");
    let reqs = reqs.lock().unwrap().clone();
    assert!(last_tool_result(&reqs[1]).contains("refused"), "{}", last_tool_result(&reqs[1]));
    if cmh::hv::sandbox::sandbox_available() {
        assert!(last_tool_result(&reqs[2]).contains("Read-only file system"), "{}", last_tool_result(&reqs[2]));
    }
    assert_eq!(std::fs::read_to_string(dir.work().join("notes/ok.txt")).unwrap(), "fine");
    for ev in journal(&dir) {
        assert!(ev["type"].is_string() && ev["ts"].is_string(), "{ev}");
    }
    // The schema refuses unknown event types and image events from a guest.
    let bad = cmh::hv::journal::check_guest_event(&json!({"type": "image_kept", "version": 9, "verdict": "x"})).unwrap_err();
    assert!(bad.contains("only by the hypervisor"), "{bad}");
    assert!(cmh::hv::journal::check_guest_event(&json!({"type": "def_use", "name": "x"})).is_err());
    assert!(cmh::hv::journal::check_guest_event(&json!({"type": "ext_note", "anything": 1})).is_ok());
}
