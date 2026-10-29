# cmh: the continual, self-modifying harness

One design, four implementations: `clojure/`, `rust/`, `racket/`, `lean/`.
Each is a coding agent that (1) rewrites parts of itself while it runs and
(2) learns across runs, keeping only the changes and lessons that hold up.

The design borrows from the harnesses surveyed in this repo's research:
samizdat (immutable kernel, rewritable loop, validate/soak/rollback gate,
verdicts distilled to durable knowledge), Prime Agent / Continual Harness
(typed, versioned supplemental state: prompt notes, lessons, skills; immutable
base prompt; rollback), autolith and oh-my-lisp (late-bound live redefinition).

## Three rings

1. **Kernel** (host code, immutable to the agent): model client, agent loop,
   journal, gate, evaluator, kernel tools. The agent can read it but no kernel
   tool writes it.
2. **Userspace** (mutable, versioned): named definitions written in the
   flavor's eval language. The kernel resolves every userspace definition **by
   name at call time** (late binding), so a newly installed version takes effect
   on the very next call without a restart.
3. **Knowledge** (durable): lessons and verdicts that carry across runs and are
   scored by outcome.

| Flavor  | Userspace language | How code becomes live at runtime |
|---------|--------------------|----------------------------------|
| Clojure | Clojure            | `load-string`/`eval` into a dedicated namespace; vars resolved by name per call |
| Racket  | Racket             | `eval` in a sandboxed namespace (`racket/sandbox`, time and memory limits) |
| Rust    | Rust               | each version is compiled by `cargo` into a cdylib and hot-loaded with `libloading`; the Rust compiler is part of the gate |
| Lean    | Lean 4             | source is elaborated in-process (`Lean.Elab` frontend) and evaluated with `evalConst`; any `theorem`s in the source must type-check, so proofs are part of the gate |

## Userspace definitions

Every definition has `name`, `kind`, `version` (1, 2, ...), `source`,
`description`, `tests`, and `status` (`trial`, `kept`, `reverted`, `rejected`).
Kinds:

- **`tool`**: exposed to the model as a tool named `name`. Contract: takes one
  JSON object (the tool input), returns a string. Also carries `input_schema`
  (JSON Schema object). The flavor fixes the exact function signature (for
  example Clojure `(defn run [input] ...)` where `input` is a keyword-keyed
  map).
- **`note`**: prompt note. `source` is plain text, appended to the supplemental
  system prompt. No evaluation, but still versioned and soaked.
- **`hook`**: kernel extension points the kernel calls by these fixed names:
  - `compose_system(notes: [string], lessons: [string]) -> string`: builds the
    *supplemental* system prompt. The kernel always prepends the immutable base
    prompt itself, so a hook cannot remove it.
  - `select_lessons(task: string, lessons: [{id, text, score}]) -> [id]`:
    picks which lessons go into the prompt for this task.

Seed userspace, shipped as source files in each flavor and installed as
`kept` version 1 when the journal is empty: both hooks with their default
behavior (`compose_system` joins notes under a "Notes" heading and lessons
under "Lessons"; `select_lessons` returns the 8 highest-scored lessons that
share at least one word of 4+ letters with the task, else the top 3 by score),
and one example tool, `word_count` (`{"text": string}` -> count as a string).

## The gate: validate, trial, keep or revert

`propose_def` is the only way userspace changes.

1. **Validate** in a scratch environment, never the live one: the source
   compiles/evaluates, defines the required entry point with the right shape,
   and every test passes. Tests are JSON: `[{"input": {...}, "expect": "..."}]`
   where `expect` must be a substring of the output (hooks get their arguments
   as `input`; for `select_lessons` compare the JSON-encoded result). Tools
   and hooks need at least one test. On failure the proposal is `rejected`, the
   live version is unchanged, and the model gets the error text.
2. **Trial**: the new version goes live immediately with status `trial`. The
   previous live version is retained.
3. **Soak**: the next `soak` uses (default 3; a hook use is one invocation by
   the kernel) are counted. Any error (exception, panic, timeout, bad return
   type) during soak reverts immediately to the previous kept version (or
   removes the definition if there was none) with status `reverted`. After
   `soak` clean uses the version becomes `kept`. Notes soak over tasks: a task
   that ends in an error counts as an error.
4. **Inherit the verdict**: every keep or revert writes a knowledge entry,
   e.g. `def word_count v2 ("counts unicode words") reverted: ArithmeticException
   on input {...}`. Verdicts are always shown to the model (they bypass
   `select_lessons`), so it does not retry a failed idea blindly.

Runtime errors from a `kept` definition are returned to the model as tool
errors but do not trigger revert. `rollback_def(name, version)` reinstalls an
older version as a new `trial` version (new version number, same source).

## Continual learning

- **Lessons**: free-text entries in knowledge with an integer `score`
  (starts 0). Created by the model through `remember`, and by reflection.
- **Reflection**: after each task finishes, the kernel makes one extra model
  call (no tools) with a compact transcript and asks for a JSON array of 0 to 3
  short, reusable lessons. Lessons are added unless they duplicate an existing
  lesson's text. Reflection can be disabled with `--no-reflect`.
- **Scoring**: each lesson that was in the prompt for a task gets +1 if the task
  succeeded and -1 if it failed. A task succeeds when the model ends with
  `end_turn` and the task had no kernel errors and no tool errors in its final
  2 tool results. Lessons at score <= -3 are retired (excluded from
  selection, still in the journal).
- **Refine**: the `/refine` command (and `cmh refine`) runs a normal agent task
  with a fixed prompt: review the recent task outcomes, verdicts, and current
  userspace, then improve userspace through `propose_def` or knowledge through
  `remember`, making at most 3 changes and preferring small ones.

## Kernel tools (always present, not redefinable)

| Tool | Input | Effect |
|------|-------|--------|
| `bash` | `command`, optional `timeout_s` (default 60) | runs in the working directory, returns combined stdout/stderr and exit code, truncated to 20 KB |
| `read_file` | `path` | file contents, truncated to 50 KB |
| `write_file` | `path`, `content` | writes the file. Refuses paths inside the state dir or the harness's own `kernel` source directory |
| `list_defs` | none | name, kind, live version, status, description for every definition |
| `show_def` | `name`, optional `version` | that version's full record including source and tests |
| `propose_def` | `name`, `kind`, `source`, `description`, `tests`, `input_schema` (tools only) | runs the gate, returns the verdict |
| `rollback_def` | `name`, `version` | see above |
| `remember` | `text` | adds a lesson |
| `recall` | `query` | lessons and verdicts whose text shares words with the query, highest score first |

Userspace tools are added to the tool list on every model request, rebuilt
from the live registry each time.

## Persistence

State dir, default `.cmh/` in the working directory, overridable with
`--state DIR`. The single source of truth is `journal.jsonl`, an append-only
log of one JSON object per line, each with `type` and `ts` (ISO-8601). Event
types: `def_proposed`, `def_rejected`, `def_trial`, `def_use` (name, version,
ok, error), `def_kept`, `def_reverted`, `lesson_added`, `lesson_scored`,
`lesson_retired`, `task_started`, `task_finished` (success flag). On startup
the kernel replays the journal to rebuild the registry (including soak
counters of trial versions), and the knowledge. Replay must reproduce the
same state that the process had; never rewrite earlier lines. The Rust
flavor may also cache build artifacts in the state dir; the Lean flavor may
cache elaborated environments. Caches must be rebuildable from the journal.

## Model provider

- `--provider anthropic` (default): `POST https://api.anthropic.com/v1/messages`
  with tool use, `anthropic-version: 2023-06-01`, `max_tokens` 32000, model from
  `--model` or `CMH_MODEL`, default `claude-sonnet-5-5`. Auth: if
  `ANTHROPIC_API_KEY` is set, send `x-api-key`. Otherwise use
  `CLAUDE_CODE_OAUTH_TOKEN`: send `authorization: Bearer <token>` and
  `anthropic-beta: oauth-2025-04-20`, and make the first system block exactly
  `You are Claude Code, Anthropic's official CLI for Claude.` (the endpoint
  requires it for OAuth tokens); the base prompt follows as the second block.
  Never print the token.
- `--provider mock --script FILE`: replays a JSON array of assistant
  `content` arrays in order, one per model request (reflection requests
  included unless `--no-reflect`), and records every request it received
  so tests can assert on the system prompt and tool list. Running out of
  script is an error.

The base system prompt (immutable, same text in all flavors) is in
`base-prompt.md` at the repository root; each flavor reads it from there at
startup, falling back to a copy embedded at build time.

## CLI

```
cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
    [--soak N] [--no-reflect] <command>

  repl            interactive; each line is a task. Slash commands:
                  /defs  /show NAME [VERSION]  /knowledge  /refine
                  /rollback NAME VERSION  /quit
  run "TASK"      one task, prints the final answer, exits 0 on success
  refine          one refine task
  defs            print the registry
  knowledge       print lessons and verdicts
```

Assistant text goes to stdout. A compact trace of tool calls and gate
verdicts goes to stderr, one line each, e.g.
`[tool] propose_def word_count -> trial v2`.

## Required tests (per flavor, native test framework, mock provider, offline)

1. Late binding: propose a new `word_count` version; the next call in the
   same process uses it.
2. Gate rejection: source that fails to compile/eval, and source that fails a
   test, are both rejected; the live version is unchanged.
3. Soak revert: a version that throws on some input reverts on that use; the
   previous version is live again; a verdict entry exists.
4. Soak keep: after `soak` clean uses the version is `kept`.
5. Persistence: a fresh kernel on the same state dir has the same live
   versions, statuses, soak counters, and knowledge.
6. Hook redefinition: a new `compose_system` changes the system prompt seen by
   the mock provider, and the base prompt is still present.
7. Lesson scoring: lessons gain and lose score with task outcomes and retire
   at -3.
8. One end-to-end mock run through `run` where the model proposes a tool and
   then calls it within the same task.
