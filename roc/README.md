# cmh, Roc flavor

The seventh implementation of [SPEC.md](../SPEC.md). The kernel is a Zig host
(`host/`, binary `cmh`) with the Roc compiler and interpreter built in. The
agent writes its userspace (tools and the two hooks) in current Roc. It builds
on the prototype in [`../roc-proto`](../roc-proto).

## Architecture

```
cmh (Zig)
├── kernel.zig     agent loop, gate, soak/keep/revert, lessons, reflection, refine
├── state.zig      registry + knowledge, rebuilt by replaying journal.jsonl
├── journal.zig    append-only journal
├── provider.zig   Anthropic Messages API (std.http, plain http ok) and the mock
├── tools.zig      bash, read_file, write_file, path rules, kernel tool schemas
├── userspace.zig  programs, the request loop, scratch validation, reports
├── rocrt.zig      the embedded compiler and the fork-per-call wall
├── roc_reference.md  Roc cheat sheet sent as the second system block
└── main.zig       CLI
seed/              word_count.roc, compose_system.roc, select_lessons.roc
tests/run_tests.py offline suite (mock provider)
```

**The wall.** At startup the host creates one `ReplSession` and publishes the
Builtin module once, which takes about 0.1 s. The parent never runs agent
code. Every call, gate check and scratchpad snippet runs in a `fork()`ed
child. The child shares the warm compiler copy-on-write, compiles one program,
writes a JSON report to a pipe and exits. The parent kills the child when a
phase passes its deadline (10 s by default, set with `CMH_ROC_TIMEOUT_MS`).
An infinite loop, a `crash`, a stack overflow or a compiler panic ends only
the child, and the model gets a structured report.

**Programs, not one big session.** A call compiles the called definition plus
the live definitions it references, followed transitively, and nothing else.
The kernel finds references by scanning identifiers. Each definition owns its
name, `name_...` helpers and `NamePascal...` types, and the gate enforces
that, so names cannot collide. Because the program is rebuilt from the
registry on every call, late binding comes for free. After `word_count` v3
goes live, an existing `shout_words` that calls it uses v3 on its next call.
Successful results are memoized by (program, expression), because Roc is pure.

### Latency measurements, and the choice

Measured with `cmh bench` on the Debug build, using realistic definitions (3
functions, a record type and a request union each, about 15 lines). The
columns are the time to add the next definition to a session, a call through
one session holding all N definitions (the prototype's model), a call that
compiles only the called definition's closure, and that closure call inside a
forked child:

| stored defs | add def | session call | closure call | closure + fork |
|---:|---:|---:|---:|---:|
| 1   | 131 ms  | 349 ms  | 278 ms | 316 ms |
| 10  | 344 ms  | 350 ms  | 269 ms | 314 ms |
| 50  | 1539 ms | 740 ms  | 275 ms | 326 ms |
| 100 | 4236 ms | 1451 ms | 382 ms | 411 ms |

With one session, call cost grows linearly, and adding a definition grows even
faster, because the session re-checks everything stored on each step. With
per-call closures, cost stays flat (the rise at 100 comes from the larger
registry scan and memory). Forking adds about 40 ms. That is why the kernel
uses closures, and also why the gate validates each proposal against only its
dependencies plus the live definitions that call it.

## Requests as data

The embedded session evaluates code at compile time and rejects every
effectful call ("Effectful Compile Time Expression"), so Roc code cannot call
the host. Tools therefore return requests:

```roc
line_count : Str, List(Str) -> [Done(Str), Fail(Str), ReadFile(Str)]
line_count = |input, results| ...   # input: tool input as JSON text
```

The host calls `NAME(input_json, [])`. If the result is a request, the host
checks it against its rules, performs it, and calls again with every result so
far, until the tool returns `Done(text)`, `Fail(msg)` or a plain `Str`. The
request set is `ReadFile`, `ReadBytes`, `ListDir`, `WriteFile` and `Bash`, plus
`Done` and `Fail`.

The rules: paths must resolve inside the work dir, with symlinks
canonicalized. `WriteFile` refuses the state dir, the whole `roc/` directory
and `base-prompt.md`. `Bash` has a 30 s timeout and a 20 KB output cap.
`ReadFile` is limited to 256 KB of UTF-8 and `ReadBytes` to 128 KB. A call can
make at most 24 requests. A refusal is not an exception. It comes back as the
next result, `ERROR: refused: ../secret.txt resolves to ..., outside the work
dir ...`, so the tool can handle it and the trace shows it.

Doing I/O this way also makes tools testable without I/O. A test can script
the results (`"responses": [...]`) and check the requests made
(`"expect_requests": [...]`).

## The gate lifecycle

`propose_def` runs these steps:

1. Shape checks: name, kind, a schema for tools, at least one test.
2. One gate child: every top-level statement must be a definition with an
   allowed name. The program (dependencies plus source) must compile. The
   source must define `NAME`. Each test runs with its own deadline. Hooks get
   their `input` as arguments, and for `select_lessons` the JSON-encoded id
   list is compared. Every live definition that calls `NAME` must still
   compile against the candidate.
3. **trial**: the candidate goes live at once.
4. **soak**: any error in the next `--soak` uses (default 3) reverts it to the
   last kept version, or removes it if there is none. That includes a type
   error, crash, timeout, `Fail`, a bad hook return or a dead child. Clean
   uses make it **kept**.
5. Every keep or revert writes a verdict, and verdicts always appear in the
   system prompt.

If a hook fails, the kernel falls back to its built-in default for that call.

Reports look like Forth's:

```
roc error [type-error] in word_count v2 (tool), line 2: Type Mismatch
<compiler report with the code snippet>
input: {...}
requests so far:
  ReadFile("a.txt") -> ...
hint: ...
```

The kinds are `parse-error`, `type-error`, `crash`, `timeout`, `child-died`,
`contract`, `wrong-output`, `fail` and `internal`. The line number is looked
up in the definition's own source or in a dependency's (`in DEP vN line L`).

**`roc_eval`** is the scratchpad. It takes `source` (optional definitions)
and `expr`, runs them in a throwaway child with the live definitions they
reference, and returns `value: <Str.inspect>` or the same report the gate
gives. Nothing is journaled.

**The cheat sheet** ([`host/roc_reference.md`](host/roc_reference.md)) was
checked against `test/snapshots` in the Roc checkout and against the compiler.
`cmh check-reference` runs every `# =>` example and compiles every definition
block, and all 68 pass.

## Pinned Roc

roc-lang/roc at `a5c6d8bf5bcb299d160f9eb5d4f953914254bf45` (2026-11-12). The
checkout is not vendored. `patch-build.py` adds a `cmh` step to its
`build.zig`, wired like `repl.wasm`. The patch block sits between marker
comments, so running the script again replaces it cleanly.

## Build, test, run

Requirements: Zig 0.17.1, python3, and a roc-lang/roc clone.

```sh
roc/build.sh [/path/to/roc]   # checkout pin, patch, zig build cmh -j2 (Debug), link roc/bin/cmh
python3 -I roc/tests/run_tests.py
roc/bin/cmh run "reverse the words of 'a b c' with a userspace tool"
roc/bin/cmh defs | knowledge | refine | repl
roc/bin/cmh check-seeds | check-reference | bench   # developer commands
```

The first build in a fresh checkout takes about 7 minutes, with a peak RSS of
1.6 GB at `-j2`. An incremental rebuild after a host change takes about 30 s
at the same peak. A ReleaseFast build was OOM-killed at 6 GB in a 7.8 GB orb,
so the flavor ships as Debug. The CLI, providers, journal format, exit codes
and the `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN`
handling match the other flavors.

## Known limitations

- **Debug build.** Evaluation is slow: about 0.3 s per call round, and 1 to
  10 s per gate run. The seed `select_lessons` takes about 7 s in the gate. A
  release build should be much faster, but it needs a machine with more
  memory.
- **Dependency scan is lexical.** A local variable named like another
  definition pulls that definition into the program. This costs compile time
  but is harmless.
- **Expected failures cannot be tested.** A tool test that ends in `Fail(...)`
  fails the gate, so error paths can be exercised only with `roc_eval`.
- **Paths are checked in the host, then opened.** A symlink swapped in
  between the check and the open is not caught, and `Bash` can break every
  rule, as in the other flavors.
- **Structured hook input only.** Hooks get exactly the SPEC arguments. There
  are no other hooks.
- **Whole-number defaults.** Unsuffixed numbers are `Dec` (shown as `3.0`).
  The cheat sheet warns about this, but models still trip on it.
