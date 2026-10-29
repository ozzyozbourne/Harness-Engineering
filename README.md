# Harness-Engineering

All custom harnesses.

## cmh: continual, self-modifying harness

One design ([SPEC.md](SPEC.md)), five implementations. Each agent rewrites its
own userspace (tools, prompt notes, and the hooks that assemble its prompt)
while it runs. Each change passes a validate, trial, and soak gate before it is
kept, and the harness carries lessons and verdicts across runs.

| Flavor | Userspace is | Made live by |
|---|---|---|
| [clojure/](clojure/) | Clojure | `eval` with late-bound vars |
| [rust/](rust/) | Rust | compiled to a cdylib by `cargo`, hot-loaded |
| [racket/](racket/) | Racket | sandboxed `eval` |
| [lean/](lean/) | Lean 4 | in-process elaboration + `evalConst`; theorems must check |
| [forth/](forth/) | a small late-bound Forth | runs inside WebAssembly (Wasmtime) with fuel, a memory cap and snapshot rollback; failures come back as structured errors |

All four share the immutable base prompt in [base-prompt.md](base-prompt.md).

Known gaps, planned improvements and experiment ideas are in [ROADMAP.md](ROADMAP.md).
