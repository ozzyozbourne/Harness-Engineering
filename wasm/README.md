# cmh, WASM flavor

The sixth implementation of [SPEC.md](../SPEC.md). Here the whole harness
runs inside WebAssembly, and the agent can rewrite all of it: not only its
userspace (tools, notes, hooks) but its own agent loop, prompts and kernel
tools. This is the "harness as a unikernel" idea. Real unikernels need KVM,
which orbs do not have, so a WebAssembly component plays the role of the
unikernel image and a small Rust program plays the role of the hypervisor.

## The idea in plain words

WebAssembly (WASM) is a portable bytecode. A WASM program runs inside a
*runtime* (here [Wasmtime](https://wasmtime.dev)) and can only touch its own
memory. Anything else, such as files, the network or the clock, must be
handed to it by the runtime as an *import*. A WASM *component* is a WASM
program plus a typed description of what it imports and exports, written in
a small interface language called WIT.

So the harness is split in two:

- **The guest (the kernel image)** is `guest/`, a Rust crate compiled for
  the `wasm32-wasip2` target into one `.wasm` component. It contains the
  agent loop, prompt assembly, the gate (validate, trial, soak, keep or
  revert), journal replay, lessons, reflection, the kernel tools and the
  Forth interpreter for userspace. It cannot do anything to the outside
  world except call the imports listed below.
- **The host (the hypervisor)** is `host/`, the `cmh` binary. It loads the
  image into Wasmtime, provides the imports, enforces limits, restores the
  image after any crash, and manages image versions. It has no agent logic.

```
 cmh (host/: the hypervisor, native Rust + Wasmtime)
 ├── model HTTP to ANTHROPIC_BASE_URL (only this origin), auth headers
 ├── bash in a sandbox (state dir, kernel source, base prompt read-only)
 ├── file writes with the protected-path rules
 ├── journal.jsonl with a stable schema
 ├── image builds, boot tests, trials, reverts
 └── limits: guest time, memory cap, trap -> report -> fresh instance
      │  WIT imports (guest/wit/cmh.wit)
      ▼
 kernel image (guest/: wasm32-wasip2 component)
 ├── agent loop, prompts, reflection, refine
 ├── gate for userspace definitions, lessons, verdicts
 ├── kernel tools: bash read_file write_file list_defs show_def forth_eval
 │   propose_def rollback_def remember recall propose_kernel list_images
 └── Forth VM (copied from forth/vm) for userspace tools and hooks
```

## The WIT world

[`guest/wit/cmh.wit`](guest/wit/cmh.wit) is the contract. It belongs to the
host: a kernel proposal cannot change it, and an image that does not match
it fails to boot.

What the image may **import** (interface `host`):

| Import | What it does |
|---|---|
| `log`, `print` | a trace line on stderr; text on stdout (repl) |
| `now-ms`, `now-iso` | the clock |
| `config` | state dir, work dir, soak, reflect, turn limit, the base prompt, the image version |
| `post-messages` | one model request. The host adds model, `max_tokens` and auth, and always puts the immutable base prompt first. It posts only to `$ANTHROPIC_BASE_URL` (or the mock) |
| `bash` | a command in the work dir with a timeout and a 20 KB output cap, inside a mount namespace where the state dir, `wasm/` and `base-prompt.md` are read-only |
| `write-file` | writes a file; refuses the state dir, `wasm/` and `base-prompt.md` |
| `journal-append`, `journal-read` | the journal, checked against the host's schema |
| `checkpoint` | saves the in-flight task so it can resume after a trap |
| `propose-image`, `images` | kernel self-modification (below) |

Files are *read* through WASI read-only "preopens": the work dir, the state
dir and `wasm/` are visible inside the image under their real paths, read
only. The image has no network and no other filesystem access.

What the image must **export**: `init` (replay the journal), `run-task`,
`resume-task` (continue after a trap), `refine`, `command` (defs, knowledge,
repl slash commands) and `self-test`.

## Limits and recovery

The host never trusts the image:

- **Guest time.** A call hook measures time spent in guest code; time inside
  host calls (model requests, bash, builds) does not count. Wasmtime's epoch
  interruption checks it every 10 ms. A stretch of 20 s without returning to
  the host, or 300 s of guest time in one call, traps with `guest-timeout` or
  `guest-time-limit`.
- **Memory.** Linear memory may not grow past 512 MB (`memory-limit`).
- **Traps become reports.** A panic, a limit, an out-of-bounds access or a
  stack overflow ends the call with a trap. The host turns it into a report
  such as `kernel trap [guest-timeout] in image v2 during run-task: ...` with
  the guest's last stderr lines (panic messages) and a hint.
- **Restore.** A trapped instance is thrown away. The host boots a fresh
  instance, which replays the journal in `init`, so it has exactly the state
  of the last journaled event. If the trap happened during a task, the host
  calls `resume-task` with the last checkpoint: each tool call of the
  interrupted turn gets the trap report as its error result, and the model
  carries on. A task is resumed at most 3 times.
- **Forth userspace** runs inside the image, with its own step budget
  (`fuel-exhausted`) and a fixed data space; Forth code cannot address any
  other memory (`bad-address`). Errors are the same structured reports as in
  the Forth flavor, and a failing trial is rolled back from an in-image
  snapshot. If something in Forth still traps the whole image, the host-level
  restore above handles it.

## Kernel images

The state dir holds an editable copy of the live image's crate in
`kernel-src/`. The agent reads it with `read_file` or `bash` and changes it
with the `propose_kernel` tool:

```json
{"description": "print a per-turn token summary",
 "edits": [{"path": "src/kernel.rs", "old_str": "...", "new_str": "..."}],
 "files": {"src/extra.rs": "..."}}
```

`edits` are exact string replacements (each `old_str` must occur exactly
once); `files` are whole contents. Only files under `src/` and `seed/` may
change. `Cargo.toml`, `Cargo.lock` and `wit/` are fixed, so a build cannot
add dependencies, build scripts or proc macros.

Lifecycle of image vN:

1. **edit**: the live image's source plus the edits, in `images/vN/src/`.
2. **compile**: `cargo build --release --locked --offline --target
   wasm32-wasip2` with 2 jobs (online only if a crate is missing), in a
   sandbox with no network where the repository (hypervisor source, evals,
   other flavors) and the state dir are read-only and `~/.config` is hidden.
   One shared target dir (`~/.cache/cmh-wasm/target`, or `$CMH_IMAGE_CACHE`)
   serves every state dir; builds take a lock.
3. **boot**: instantiate in a scratch instance (temporary state and work
   dirs, a mock model, no bash) and run `init`.
4. **self-test**: the image's own `self-test` export runs its built-in mock
   scenarios in memory.
5. **conformance**: the host's own mock task checks that SPEC behaviour still
   holds: the base prompt goes first, the kernel tools are listed, a seed
   tool works, a proposed tool is live within the same task, the journal has
   the right events, and a second instance replaying the journal reaches the
   same state.
6. **trial**: the image becomes live from the next task on, for `--soak`
   tasks (default 3). After that many clean tasks it is **kept**. It is
   **reverted** to the last kept image if it traps, fails to boot, or a task
   fails during the trial.

Every step is journaled as `image_proposed`, `image_rejected` (with the
stage and the compiler or test output), `image_trial`, `image_use`,
`image_kept` and `image_reverted`. Keep, revert and reject verdicts are
knowledge, shown to the model like definition verdicts, for example
`image v3 ("...") rejected at compile: error[E0308]: mismatched types`.
`cmh images` (or `/images`, or the `list_images` tool) shows the history.

**The journal outlives every image.** The host owns its schema: images may
write only the SPEC event types (with their required fields) or free-form
`ext_*` events, and only the host writes `image_*` events. Every event is
stamped with the image version that wrote it. Any image, old or new, can
therefore replay the same journal; unknown types are ignored.

## Layout

```
wasm/
  Cargo.toml            workspace: host (guest is its own workspace)
  guest/                the kernel image (wasm32-wasip2 component)
    wit/cmh.wit         the WIT world (host-owned)
    src/lib.rs          exports
    src/kernel.rs       agent loop, gate, prompts, tools dispatch
    src/forth/          Forth VM (from forth/vm) and report formatting
    src/selftest.rs     the image's built-in mock scenarios
    src/sys.rs          imports (HostSys) and the in-memory test world (MemSys)
    src/state.rs        journal replay
    src/tools.rs        kernel tool schemas, bash/read/write
    seed/*.fs           seed userspace
  host/                 the hypervisor, binary `cmh`
    build.rs            builds and embeds the built-in image and its source
    src/hv/guest.rs     Wasmtime: imports, limits, traps
    src/hv/images.rs    image lifecycle and builds
    src/hv/journal.rs   journal and its schema
    src/hv/sandbox.rs   bash and file-write capabilities
    src/hv/mod.rs       boot, tasks, trap recovery
    tests/              SPEC, Forth and image tests
```

## Build, test, run

```sh
rustup target add wasm32-wasip2
cd wasm
cargo build --release           # host/build.rs builds guest/ and embeds it
cargo test                      # offline, mock provider; image tests run real cargo builds
./target/release/cmh run "reverse the string 'stressed' with a userspace tool"
./target/release/cmh defs
./target/release/cmh knowledge
./target/release/cmh images
./target/release/cmh repl
```

`.cargo/config.toml` limits builds to 2 jobs. The CLI, the providers
(`anthropic`, and `mock --script FILE`, which logs requests to
`STATE/mock-requests.jsonl`), `ANTHROPIC_BASE_URL` (plain `http://` works),
`ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, the journal format and the
exit codes (0 success, 1 failure, 2 usage) are the same as in the other
flavors. The state dir defaults to `.cmh/`.
