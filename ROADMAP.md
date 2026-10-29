# Gaps, improvements and experiments

Where the five cmh harnesses (Clojure, Rust, Racket, Lean, Forth-in-WASM) fall
short of the goal, how to close the gaps, and what to try next. Written on
2026-10-29 from the build reports, the test suites and the eval runs in
[evals/](evals/).

The goal, in one sentence: a substrate the agent can program itself on, which
never crashes from the agent's code, answers every mistake with precise
feedback, enforces clear rules, and keeps only the changes that make the agent
better.

## Where things stand

| | Clojure | Rust | Racket | Lean | Forth (WASM) |
|---|---|---|---|---|---|
| Score vs Prime, DeepSeek V4.1 Flash | 102% | 102% | 102% | 98% | 114% |
| Score vs Prime, GLM 5.3 Flash | 116% | 113% | 118% | 116% | not run |
| Agent code can crash the harness | yes (no sandbox) | yes (in-process native code) | no (sandbox) | partly (`panic!` not caught) | no (WASM wall) |
| Runaway loop is actually stopped | no (thread keeps running) | no (thread abandoned) | yes (sandbox limit) | no (thread keeps spinning) | yes (fuel) |
| Scratchpad for trying code | no | no | no | no | yes (`forth_eval`) |

Most of the score comes from the shared design (lessons, reflection, the
gate), not from self-modification: across about 60 episodes per flavor the
agents proposed only 5 to 78 changes. The evals do not yet reward
self-modification (see "Gaps in the evals").

## Gaps in all five harnesses

### 1. Safety: the wall is incomplete

- **Agent code can take down the process** in Clojure, Rust and Lean. Rust
  loads userspace as native code in-process, so a segfault or `abort` kills
  everything; Lean cannot catch `panic!`; Clojure has only a timeout.
- **Timeouts do not stop runaway code** in Clojure, Rust and Lean. The call is
  abandoned but its thread keeps burning CPU until the process exits.
- **`bash` can break every rule.** `write_file` refuses the kernel and the
  journal, but `bash` can edit anything, including the kernel source, the
  journal and the eval checker's inputs. In chess terms, the agent can flip
  the board.
- **Old versions leak.** Every flavor keeps superseded code loaded (namespaces,
  `.so` files, evaluators, Forth data space) until the process exits.

**Improvements**

- Run userspace for Clojure, Rust and Lean out of process, in a forked worker
  or a WASM module, so a crash or runaway loop only kills the worker. Rust can
  compile userspace to `wasm32` and reuse the Forth host's Wasmtime wall
  instead of loading `.so` files.
- Run `bash` in a separate sandbox (bubblewrap or a container) with a
  read-only view of the kernel, the state dir and `base-prompt.md`, plus CPU,
  memory and time limits. Today only the eval runner's watchdog limits it.
- Reclaim old versions: reload workers or VM instances periodically from the
  journal so leaked code is dropped.

### 2. Back pressure: feedback says "it failed", rarely "why" and "what next"

- Only the Forth flavor returns structured errors (word, version, stack,
  input). The other four mostly return raw exception or compiler text.
- Errors from a kept definition are returned but do not teach anything: they
  do not become lessons or verdicts.
- The gate checks only the agent's own tests. An agent can write a test that
  agrees with a wrong implementation.

**Improvements**

- One error format for every flavor: what failed, the definition and version,
  the input, the stack or backtrace, the rule involved, and a suggested next
  step. Port the Forth report format.
- Add held-out checks to the gate: property tests and fuzzed inputs generated
  by the kernel from the tool's `input_schema`, in addition to the agent's
  tests.
- Turn repeated runtime errors of kept definitions into a verdict and a
  prompt nudge ("`word_count` v2 failed 3 times on empty input").

### 3. Selection: changes are kept for not crashing, not for helping

- A trial is kept after 3 uses without an error. Nothing measures whether the
  change made tasks faster, cheaper or more accurate.
- Task success is self-reported: the model stops cleanly and the last tool
  results had no errors. That is easy to satisfy and easy to game (Prime's
  paper reports an agent saving a Factorio cheat as a skill).
- Lesson credit is crude: every lesson in the prompt gets +1 or -1 for the
  whole task, whether it mattered or not.

**Improvements**

- Add a fitness score per task (success from an independent checker when one
  exists, tokens, wall time, tool errors) and keep a change only if the score
  over its trial window is not worse than the previous version's, as in
  samizdat's RFC-010.
- Let tasks carry an external verifier (a command or a checker script) that the
  kernel runs, so success is not self-reported.
- Credit lessons by use: ask the model at reflection time which lessons it
  actually applied, or A/B a lesson by withholding it on some tasks.

### 4. Scope of self-modification is narrow

- The agent can change tools, notes and two hooks (`compose_system`,
  `select_lessons`). It cannot change the agent loop, the reflection prompt,
  the compaction policy, or how tools are chosen.
- Proposals cannot depend on each other cleanly. In Lean each proposal is
  elaborated against the base environment, so a macro defined in one tool is
  not visible to the next. Rust userspace can only use `std` and
  `serde_json`.

**Improvements**

- Make the agent loop data, as samizdat does: a manifest of steps (build
  prompt, call model, run tools, reflect, compact) where each step is a hook
  the agent can redefine behind the same gate.
- Add hooks for reflection, compaction and tool selection.
- Chain environments in Lean (kept definitions form a prelude for later ones)
  so macros and helper definitions accumulate into a DSL. The same for the
  Forth dictionary across processes, through image snapshots.

### 5. Missing developer tools for the agent

- **Scratchpad.** Only Forth has one (`forth_eval`), and it cut that flavor's
  rejected proposals from 69 to 0 and its tokens from 7.8M to 3.7M. The other
  four flavors make the agent propose a definition just to try code.
- **No diff or history view** of a definition across versions beyond
  `show_def`.
- **No way to test a change against past tasks.** The journal records tasks
  but the agent cannot replay them.

**Improvements**

- Add `eval` scratchpads to Clojure (`load-string` in a throwaway namespace),
  Racket (a throwaway sandbox evaluator), Lean (elaborate in a scratch
  environment and `#eval`), and Rust (compile and run in a throwaway worker).
- Add `diff_def name v1 v2` and a `history` tool.
- Add `replay` that reruns a past task with the current userspace on a copy of
  its working directory, so the agent can check a change against real work.

### 6. Continual learning is shallow

- Lessons are free text with a score; there is no deduplication beyond exact
  text, no merging, and no forgetting other than retirement at -3.
- Nothing is shared between projects or flavors.
- Reflection runs after every task, even trivial ones, and adds up to 3
  lessons each time, so the store fills with near-duplicates.

**Improvements**

- Consolidate periodically: cluster similar lessons, merge them, and drop the
  ones that were never selected.
- Separate scopes, like Prime's local and global harness state: project
  lessons, personal lessons, and global lessons, with promotion between them.
- Reflect only when a task failed, took unusually many turns, or used a tool
  for the first time.

### 7. Operational gaps

- No streaming responses, so long model calls show nothing until they finish.
- No token or cost accounting inside the harnesses; only the eval proxy
  measures it.
- No concurrency: one task at a time, no subagents (Prime spawns them).
- The 40-turn limit per task is hard-coded.

## Gaps in the evals

- **They do not reward self-modification.** Almost every task is solvable with
  `bash` alone, so a harness that never changes itself scores the same as one
  that does.
- **Small samples.** Two trials per suite; differences of a few points are
  noise.
- **Prime ran with mostly default settings.** Its auto-refine and global
  refinement were not configured specifically for the learning suites.
- **No control run yet.** `--fresh-state` (wipe memory between episodes)
  exists but was never run, so the learning-transfer numbers are not yet
  separated from what the model gets right without memory.
- **Only two models.** GLM 5.3 Flash and DeepSeek V4.1 Flash, both small and
  fast.

**Improvements**

- Run the `--fresh-state` control for all six harnesses.
- Add 3 or more trials and report confidence intervals.
- Add the suites below that reward building and reusing tools.

## Experiments to try next

1. **Tool-reuse suite.** Ten episodes where an early tool makes later tasks
   much cheaper (for example a custom binary format the agent must parse
   repeatedly, or a slow manual procedure that a tool turns into one call).
   Measure tokens and time per episode across the sequence. This is the first
   eval that would show which language actually helps self-improvement.
2. **Fresh-state control.** Rerun all suites with memory wiped between
   episodes. The gap between normal and fresh runs is the real value of
   continual learning.
3. **Scratchpads everywhere.** Add an `eval` tool to the other four flavors
   and measure how proposals, rejections and tokens change, as they did for
   Forth.
4. **Out-of-process userspace.** Move Clojure, Rust and Lean userspace into
   workers or WASM and measure the overhead per call against the safety gain.
5. **Fitness-based keeping.** Replace "3 clean uses" with "score not worse than
   the previous version" and see whether fewer but better changes stick.
6. **Agent loop as data.** Let the agent rewrite its own loop steps (the
   samizdat idea) in one flavor and watch what it changes over 50 tasks.
7. **Adversarial back pressure.** Plant tasks that tempt reward hacking (a
   checker that can be edited, a shortcut that breaks a rule) and measure
   which harness lets the cheat persist. Prime's Factorio failure is the
   reference case.
8. **Bash flavor.** Functions are late-bound and `declare -f` plus `eval` give
   live redefinition; a quick build, and a useful low end for comparison.
9. **Compile-time Rust generations.** Each kept change triggers a rebuild
   whose `build.rs` reads the journal; the core then execs into the new
   binary. Compare the cost and safety with in-process hot loading.
10. **Accumulating Lean prelude with proofs.** Let kept definitions and their
    theorems form a prelude for later proposals, and require new versions to
    prove equivalence or a stated property against the previous one.
11. **Unikernel or microVM per generation.** Boot each kept generation as a
    Unikraft or Firecracker image and roll back by booting the previous one.
    Needs a runner with KVM; orbs do not have it.
12. **Model sweep.** Run the same suites with stronger and weaker models to see
    whether harness differences grow or shrink as models improve.
13. **Long-horizon run.** One harness, one project, 200 tasks over days, and
    track whether tokens per task fall as userspace and lessons accumulate.

## Suggested order

1. Fresh-state control (cheap, makes every learning number trustworthy).
2. Scratchpads for the other four flavors (small change, large measured
   effect in Forth).
3. Sandboxed `bash` and out-of-process userspace (closes the safety gaps).
4. The tool-reuse suite (the eval that can actually rank the languages).
5. Fitness-based keeping and an external task verifier.
6. Agent loop as data, then the longer experiments.
