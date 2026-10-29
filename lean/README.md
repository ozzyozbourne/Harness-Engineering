# cmh (Lean 4 flavor)

A coding agent that rewrites parts of itself while it runs and learns across runs.
It implements [`../SPEC.md`](../SPEC.md): an immutable kernel, versioned userspace written in
Lean 4 and gated by validate, trial, soak, then keep or revert, and durable lessons and verdicts.
All state is replayed from `.cmh/journal.jsonl`.

## How self-modification works here

- `propose_def` wraps the source in `namespace Cmh.Def.<name>.v<N> … end` and appends a
  kernel-written entry declaration with the contract type, for example
  `def __cmh_entry : Lean.Json → IO String := run`. The kernel then elaborates the result
  **in-process** with `Lean.Elab.process`. It does this against a scratch copy of an environment
  imported once per process with `importModules #[Init, Lean.Data.Json]`, and sets
  `maxHeartbeats` to 200000.
- If elaboration succeeds, `Environment.evalConst` turns `__cmh_entry` into a native closure
  that the interpreter runs. The tests run against that closure before anything goes live.
- Contracts: a tool defines `def run (input : Lean.Json) : IO String`. The hooks define
  `def composeSystem (notes lessons : List String) : String` and
  `def selectLessons (task : String) (lessons : List (Nat × String × Int)) : List Nat`.
  A note is plain text.
- **Proofs are part of the gate.** Every `theorem` and `example` in a proposal must check, so a
  proposal can carry machine-checked claims about its own code, for example
  `theorem countWords_empty : countWords "" = 0 := rfl`. A false claim rejects the proposal just
  as a failing test does. To keep that guarantee meaningful, the kernel rejects any source that
  contains `sorry`, `sorryAx`, `axiom`, `unsafe`, `implemented_by`, `extern`, `import`,
  `initialize`, `#eval` or `#print`, and any elaboration message that mentions `sorry`. This is a
  token check, so the words are also refused inside strings and comments.
- The kernel resolves every userspace call by name in the registry on each call (late binding).
  Compiled closures are cached per `(name, version)`, and the cache is rebuilt lazily from
  journal sources.
- Userspace calls run on a dedicated thread with a 30 s wall-clock limit. Exceptions and timeouts
  count as errors, and an error reverts a trial version.

## Build, test, run

Requires elan with `leanprover/lean4:v4.35.0` (pinned in `lean-toolchain`) and `curl`. At runtime
the kernel finds the toolchain `.olean` files with `lean --print-prefix` or `LEAN_SYSROOT`.

```sh
cd lean
lake build                     # builds .lake/build/bin/cmh
lake test                      # or: lake exe cmh-test   (8 required tests, mock provider, offline)
.lake/build/bin/cmh run "count the words in README.md"
```

## CLI

```
cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    [--soak N] [--no-reflect] <command>
  repl | run "TASK" | refine | defs | knowledge
  repl slash commands: /defs /show NAME [VERSION] /knowledge /refine /rollback NAME VERSION /quit
```

- `run` writes only the final answer to stdout and exits 0 on success, 1 on failure, and 2 on a
  usage error. The trace (`[tool] …`, `[gate] …`, `[task] …`) goes to stderr.
- Anthropic auth comes from `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`. With an OAuth token
  the kernel sends a Bearer header, the oauth beta header, and the Claude Code system block first.
  Requests go to `$ANTHROPIC_BASE_URL/v1/messages` (default `https://api.anthropic.com`; `http://`
  also works). The model comes from `--model`, then `CMH_MODEL`, then the default
  `claude-sonnet-5-5`. The kernel calls `curl` as a subprocess and passes all headers through
  `curl -K -` on stdin, so the token is never in argv.

## Layout

`Cmh/Elab.lean` handles runtime elaboration and `evalConst`. `Cmh/State.lean` holds the journal
and replay. `Cmh/Kernel.lean` contains the gate, hooks, tools, agent loop, reflection and scoring.
`Cmh/Provider.lean` has the anthropic and mock providers. `Cmh/Cli.lean` is the CLI.
`userspace/` holds the seed sources (also embedded at build time), and `Test.lean` holds the tests.
