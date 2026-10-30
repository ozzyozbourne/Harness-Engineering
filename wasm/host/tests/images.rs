//! Kernel image tests: proposals are really built with cargo for
//! wasm32-wasip2 (in the shared image build area), booted, self-tested and
//! put on trial. Builds are serialized by a lock, so these tests mostly
//! wait for each other.

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MARKER: &str = "IMAGE-TEST-MARKER";

fn edit(path: &str, old: &str, new: &str, description: &str) -> Value {
    json!({"description": description, "edits": [{"path": path, "old_str": old, "new_str": new}]})
}

/// A good image: one extra line in the kernel's own system block.
fn marker_image() -> Value {
    edit(
        "src/kernel_reference.md",
        "# Your kernel (kernel-owned)",
        &format!("# Your kernel (kernel-owned) {MARKER}"),
        "adds a marker to the kernel prompt",
    )
}

fn has_marker(req: &Value) -> bool {
    system_texts(req).iter().any(|s| s.contains(MARKER))
}

fn rec(hv: &cmh::Hypervisor, v: u32) -> cmh::hv::images::ImageRec {
    hv.image_recs().into_iter().find(|r| r.version == v).expect("image record")
}

fn image_verdict(hv: &mut cmh::Hypervisor, v: u64) -> String {
    verdicts(hv)
        .into_iter()
        .rev()
        .find(|x| x["name"] == "image" && x["version"] == v)
        .map(|x| x["text"].as_str().unwrap().to_string())
        .unwrap_or_default()
}

#[test]
fn good_image_is_used_next_task_kept_after_soak_and_booted_by_a_second_process() {
    let dir = TestDir::new("img-good");
    let (mut hv, reqs) = open(&dir, vec![text("one"), text("two")], 2);
    let verdict = hv.propose_image(&marker_image()).unwrap();
    assert!(verdict.starts_with("trial image v2"), "{verdict}");
    assert!(verdict.contains("self-test scenarios passed") && verdict.contains("conformance passed"), "{verdict}");
    assert_eq!(hv.live_image(), 2);
    let src = std::fs::read_to_string(dir.state().join("kernel-src/src/kernel_reference.md")).unwrap();
    assert!(src.contains(MARKER), "kernel-src follows the live image");

    assert!(hv.run_task("first task on v2").success);
    assert_eq!(hv.running_image(), Some(2));
    assert!(has_marker(&reqs.lock().unwrap()[0]), "the next task runs on the new image");
    assert_eq!((rec(&hv, 2).status.as_str(), rec(&hv, 2).clean_tasks), ("trial", 1));
    assert!(hv.run_task("second task on v2").success);
    assert_eq!(rec(&hv, 2).status, "kept");
    assert!(image_verdict(&mut hv, 2).contains("kept after 2 clean tasks"), "{}", image_verdict(&mut hv, 2));
    let kinds: Vec<String> =
        journal(&dir).iter().filter_map(|e| e["type"].as_str().map(String::from)).filter(|k| k.starts_with("image_")).collect();
    assert_eq!(kinds, ["image_proposed", "image_trial", "image_use", "image_use", "image_kept"]);
    drop(hv);

    // A second process replays the journal and boots the kept image.
    let (mut hv2, reqs2) = open(&dir, vec![text("three")], 2);
    assert_eq!(hv2.running_image(), Some(2));
    assert!(hv2.run_task("third").success);
    assert!(has_marker(&reqs2.lock().unwrap()[0]));
    assert!(hv2.images_table().contains("*v2"), "{}", hv2.images_table());
}

#[test]
fn image_that_fails_to_compile_is_rejected_with_the_compiler_error() {
    let dir = TestDir::new("img-compile");
    let (mut hv, _) = open(&dir, vec![], 3);
    let req = edit(
        "src/kernel.rs",
        "pub const REFINE_PROMPT: &str",
        "const BROKEN: u32 = \"not a number\";\npub const REFINE_PROMPT: &str",
        "broken",
    );
    let err = hv.propose_image(&req).unwrap_err();
    assert!(err.starts_with("rejected at compile"), "{err}");
    assert!(err.contains("error[E0308]: mismatched types") && err.contains("src/kernel.rs"), "{err}");
    assert!(err.contains("The live kernel is unchanged (image v1)"), "{err}");
    assert_eq!(hv.live_image(), 1);
    assert_eq!(rec(&hv, 2).status, "rejected");
    assert!(image_verdict(&mut hv, 2).contains("rejected at compile: error[E0308]"), "{}", image_verdict(&mut hv, 2));

    // Edits that do not apply, and files outside src/ and seed/, never reach the compiler.
    let err = hv.propose_image(&edit("src/kernel.rs", "this text is not in the file", "x", "bad edit")).unwrap_err();
    assert!(err.contains("rejected at edit") && err.contains("old_str not found"), "{err}");
    let err = hv.propose_image(&json!({"description": "deps", "files": {"Cargo.toml": "[package]"}})).unwrap_err();
    assert!(err.contains("cannot be changed"), "{err}");
    let err = hv.propose_image(&json!({"description": "escape", "files": {"src/../../x.rs": ""}})).unwrap_err();
    assert!(err.contains("without `..`"), "{err}");
    assert_eq!(hv.live_image(), 1);
}

#[test]
fn image_that_traps_at_boot_is_rejected() {
    let dir = TestDir::new("img-boot");
    let (mut hv, _) = open(&dir, vec![], 3);
    let old = "        let k = kernel::Kernel::open(Rc::new(sys::HostSys))?;";
    let new = "        if std::hint::black_box(true) {\n            panic!(\"boom at boot\");\n        }\n        let k = kernel::Kernel::open(Rc::new(sys::HostSys))?;";
    let err = hv.propose_image(&edit("src/lib.rs", old, new, "panics at boot")).unwrap_err();
    assert!(err.starts_with("rejected at boot"), "{err}");
    assert!(err.contains("kernel trap [guest-panic]") && err.contains("boom at boot"), "{err}");
    assert_eq!(hv.live_image(), 1);
    assert_eq!(rec(&hv, 2).status, "rejected");
}

#[test]
fn image_that_fails_its_self_test_is_rejected() {
    let dir = TestDir::new("img-selftest");
    let (mut hv, _) = open(&dir, vec![], 3);
    // Userspace tools silently vanish from the tool list.
    let old = r#"for (def, v) in self.state.live_defs().filter(|(_, v)| v.kind == "tool") {"#;
    let new = r#"for (def, v) in self.state.live_defs().filter(|(_, v)| v.kind == "toolz") {"#;
    let err = hv.propose_image(&edit("src/kernel.rs", old, new, "drops userspace tools")).unwrap_err();
    assert!(err.starts_with("rejected at self-test"), "{err}");
    assert!(err.contains("late-binding task") && err.contains("tool word_count missing"), "{err}");
    assert_eq!(hv.live_image(), 1);
}

#[test]
fn trial_image_is_reverted_after_a_failed_task_and_not_used_next_run() {
    let dir = TestDir::new("img-revert");
    // The script is empty: the task fails.
    let (mut hv, _) = open(&dir, vec![], 3);
    hv.propose_image(&marker_image()).unwrap();
    assert_eq!(hv.live_image(), 2);
    assert!(!hv.run_task("this task fails").success);
    assert_eq!(hv.live_image(), 1);
    let r = rec(&hv, 2);
    assert_eq!(r.status, "reverted");
    assert!(r.reason.contains("task 1 failed during its trial"), "{}", r.reason);
    assert!(image_verdict(&mut hv, 2).contains("reverted: task 1 failed during its trial"));
    assert!(!std::fs::read_to_string(dir.state().join("kernel-src/src/kernel_reference.md")).unwrap().contains(MARKER));
    drop(hv);

    let (mut hv2, reqs) = open(&dir, vec![text("ok")], 3);
    assert_eq!(hv2.running_image(), Some(1));
    assert!(hv2.run_task("next").success);
    assert!(!has_marker(&reqs.lock().unwrap()[0]), "the reverted image is not used");
}

#[test]
fn host_survives_guest_infinite_loop_and_memory_blowup() {
    let dir = TestDir::new("img-chaos");
    let script = vec![tool_use("s", "chaos_spin", json!({})), text("recovered")];
    let (mut hv, reqs) = open(&dir, script, 3);
    let spin = "{ let mut n = 0u64; loop { n = std::hint::black_box(n.wrapping_add(1)); } }";
    let blowup = "{ let mut v: Vec<Vec<u8>> = Vec::new(); loop { v.push(std::hint::black_box(vec![7u8; 1 << 20])); } }";
    let req = json!({"description": "chaos commands", "edits": [
        {"path": "src/kernel.rs", "old_str": "            other => Err(format!(\"unknown command `{other}`\")),",
         "new_str": format!("            \"spin\" => {spin}\n            \"blowup\" => {blowup}\n            other => Err(format!(\"unknown command `{{other}}`\")),")},
        {"path": "src/kernel.rs", "old_str": "            \"list_images\" => Ok(sys.images()),",
         "new_str": format!("            \"list_images\" => Ok(sys.images()),\n            \"chaos_spin\" => {spin}")},
    ]});
    let verdict = hv.propose_image(&req).unwrap();
    assert!(verdict.starts_with("trial image v2"), "{verdict}");

    let t0 = Instant::now();
    let err = hv.command("/spin").unwrap_err();
    assert!(err.contains("kernel trap [guest-timeout] in image v2 during command"), "{err}");
    assert!(t0.elapsed() < Duration::from_secs(30));
    assert!(hv.command("defs").unwrap().contains("word_count"), "a fresh instance serves the next call");

    let err = hv.command("/blowup").unwrap_err();
    assert!(err.contains("kernel trap [memory-limit] in image v2"), "{err}");
    assert!(hv.command("defs").unwrap().contains("word_count"));

    // A trap inside a task: the trial image is reverted, a fresh instance of
    // v1 replays the journal, and the model gets the report as the tool result.
    let out = hv.run_task("call the spinning tool");
    assert_eq!(out.answer, "recovered");
    let reqs = reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    let result = last_tool_result(&reqs[1]);
    assert!(result.contains("kernel trap [guest-timeout] in image v2 during run-task") && result.contains("restored"), "{result}");
    assert_eq!(hv.live_image(), 1);
    assert!(rec(&hv, 2).reason.starts_with("trapped: kernel trap [guest-timeout]"), "{}", rec(&hv, 2).reason);
    assert_eq!(hv.running_image(), Some(1));
    let tasks = state(&mut hv)["tasks"].as_array().unwrap().clone();
    assert_eq!(tasks.len(), 1, "the resumed task is still one task");
    assert_eq!(tasks[0]["summary"], "recovered");
}

#[test]
fn propose_kernel_tool_end_to_end_and_second_process_boots_the_trial() {
    let dir = TestDir::new("img-e2e");
    let mut req = marker_image();
    req["description"] = json!("marker via the tool");
    let script = json!([tool_use("k", "propose_kernel", req), text("proposed")]);
    let (code, stdout, stderr) = run_bin(&dir, &script, &["run", "change your kernel"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "proposed\n");
    assert!(stderr.contains("[tool] propose_kernel marker via the tool -> trial image v2"), "{stderr}");
    assert!(stderr.contains("[image] v2 built in"), "{stderr}");
    let (_, table, _) = run_bin(&dir, &json!([]), &["images"]);
    assert!(table.contains("*v2") && table.contains("trial (0/3 clean tasks)"), "{table}");

    let (code, _, stderr) = run_bin(&dir, &json!([text("hello")]), &["run", "hi"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[hv] booted kernel image v2"), "{stderr}");
    assert!(has_marker(mock_log(&dir).last().unwrap()));
}
