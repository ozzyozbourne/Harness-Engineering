# cmh (Rust flavor)

A coding agent that rewrites parts of itself while it runs and learns across
runs. It implements [`../SPEC.md`](../SPEC.md): an immutable kernel
(`src/kernel/`), a versioned userspace written in Rust, and durable knowledge
(lessons and verdicts), all rebuilt from an append-only `journal.jsonl`.

## How self-modification works here

Each userspace definition version (a tool or a hook) becomes a small generated
cdylib crate under `<state>/build/<name>_v<N>/`:

- `src/user.rs` holds the agent's source unchanged.
- `src/lib.rs` is a kernel-written wrapper that exports C-ABI `cmh_call` /
  `cmh_free` functions. They pass JSON strings in and out, and `catch_unwind`
  turns panics into errors.

The contracts are:

```rust
pub fn run(input: &serde_json::Value) -> Result<String, String>                // tool
pub fn compose_system(notes: &[String], lessons: &[String]) -> String           // hook
pub fn select_lessons(task: &str, lessons: &[serde_json::Value]) -> Vec<i64>    // hook
```

`propose_def` first runs `cargo build` (offline first, with one online retry if
needed), then loads the `.so` with `libloading` and runs the tests against it.
A trial only goes live after both pass. The Rust compiler is part of the gate:
type errors, wrong signatures and borrow errors reject a proposal before it can
run, and the trimmed compiler output goes back to the model.

The kernel looks up every definition in the registry by name on each call, so a
new version takes effect on the very next call. Each call runs on a worker
thread with a timeout. Old libraries are never unloaded; they are leaked on
purpose. All generated crates share one `CARGO_TARGET_DIR`
(`<state>/target`, or `$CMH_TARGET_DIR`), so `serde_json` compiles once, and a
typical proposal builds in about 0.1–0.3 s after that. Build output is only a
cache: a missing library is rebuilt from the source stored in the journal.

## Build, test, run

```sh
cargo build --release          # binary: target/release/cmh
cargo test                     # 8 spec tests + provider unit tests (mock provider, offline)
cargo clippy --all-targets
```

```
cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    [--soak N] [--no-reflect] <command>

  repl            interactive; slash commands /defs /show NAME [V] /knowledge
                  /refine /rollback NAME V /quit
  run "TASK"      one task; stdout is only the final answer; exit 0 on success, 1 on failure
  refine          one refine task
  defs            print the registry
  knowledge       print lessons and verdicts
```

The trace (tool calls, gate verdicts, assistant text in `run`) goes to stderr.
Environment variables:

- Auth: `ANTHROPIC_API_KEY` (sends `x-api-key`), or else
  `CLAUDE_CODE_OAUTH_TOKEN` (sends a Bearer token plus the OAuth beta header,
  with the Claude Code identity as the first system block).
- `ANTHROPIC_BASE_URL`: default `https://api.anthropic.com`; a plain `http://`
  URL also works.
- `CMH_MODEL`: default `claude-sonnet-5-5`.
- `CMH_TARGET_DIR`: shared build cache.

The mock provider also appends every request it receives to
`<state>/mock-requests.jsonl`.
