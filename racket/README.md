# cmh, Racket flavor

A coding agent that rewrites parts of itself while it runs and learns across
runs. This directory implements [`../SPEC.md`](../SPEC.md) in Racket 9 CS,
using only the standard distribution (`racket/sandbox`, `json`,
`net/http-client`, `rackunit`).

## How self-modification works here

- **Kernel** (`kernel/`): the agent loop, the gate, the journal, the providers,
  and the kernel tools. `write_file` refuses paths under `kernel/`, the state
  dir, and `../base-prompt.md`.
- **Userspace** (`seed/` gives the v1 sources): Racket source evaluated with
  `eval` inside `racket/sandbox` evaluators (`make-evaluator 'racket/base`,
  10 s and 128 MB per evaluation, no file writes or network). Entry points:
  - tool: `(define (run input) ...)`. `input` is a jsexpr hasheq with symbol
    keys. The tool returns a string.
  - hook `compose_system`: `(define (compose-system notes lessons) ...)`, which
    takes lists of strings and returns a string.
  - hook `select_lessons`: `(define (select-lessons task lessons) ...)`.
    Lessons are hasheqs with `'id 'text 'score`. The hook returns a list of ids.
- **Validation** happens in a fresh scratch evaluator that is then killed. The
  source must evaluate, define the entry point with the right arity, and pass
  every test. The live evaluators are never touched.
- **Late binding**: each live version has its own evaluator, cached by
  `(name . version)`. On every call the kernel looks the name up in the
  registry, takes the live version's evaluator, and calls the entry point. A
  new trial is therefore used on the very next call, and a revert takes effect
  at once. Only plain data crosses the sandbox boundary: inputs are quoted
  jsexprs, and results are type-checked (string, or list of integer ids). A
  wrong result type counts as an error.
- **Persistence**: `journal.jsonl` is replayed through the same `apply-event!`
  that live changes use. Evaluators are a cache and are rebuilt lazily.

## Run

```sh
raco make racket/main.rkt            # compile once for fast startup
racket/bin/cmh run "TASK"            # or: racket racket/main.rkt run "TASK"
```

```
cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    [--soak N] [--no-reflect] repl | run "TASK" | refine | defs | knowledge
```

- `run` writes only the final answer to stdout. It exits 0 on success, 1 on
  failure, and 2 on a usage error.
- The trace (`[tool] ...`, `[gate] ...`, `[task] ...`) goes to stderr.
- REPL slash commands: `/defs`, `/show NAME [VERSION]`, `/knowledge`,
  `/refine`, `/rollback NAME VERSION`, `/quit`.
- Authentication uses `ANTHROPIC_API_KEY`, or else `CLAUDE_CODE_OAUTH_TOKEN`
  (Bearer token, the OAuth beta header, and the Claude Code identity as the
  first system block).
- `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com`) accepts `http://`
  as well as `https://`, so a local logging proxy works.
- The model comes from `--model`, then `CMH_MODEL`, and defaults to
  `claude-sonnet-5-5`.

## Test

```sh
raco test racket/
```

The suite runs offline against the mock provider. It covers the 8 required
SPEC tests, plus the provider wire format, plain-HTTP base URLs, and the
`write_file` refusals.
