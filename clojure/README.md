# cmh: Clojure flavor

A coding agent that rewrites parts of itself while it runs and learns across
runs. This directory implements [`../SPEC.md`](../SPEC.md) in Clojure.

- **Kernel** (`src/cmh/kernel/`, immutable to the agent): model client,
  agent loop, journal, gate, evaluator, kernel tools, CLI. `write_file`
  refuses this directory and the state dir.
- **Userspace** (`seed/` holds the version-1 sources): Clojure tools, notes
  and hooks, changed only through `propose_def`.
- **Knowledge**: lessons (scored, retired at -3) and gate verdicts, kept in
  `<state>/journal.jsonl`.

## How self-modification works here

- **Scratch validation**: a proposal's source is loaded with `load-string`
  into a new namespace `cmh.scratch.<name>.<uuid>` (`create-ns` +
  `refer-clojure`, with `*ns*` bound to it). The kernel checks that the entry
  var exists and is a fn, runs the tests against it, and then always calls
  `remove-ns`. Nothing touches the live namespaces.
- **Late binding**: each live version gets its own namespace,
  `cmh.user.<name>.v<N>-<source-hash>`. The kernel never keeps a
  function reference. On every call it reads the live version number from the
  registry (an atom rebuilt from the journal), finds that namespace with
  `find-ns` (loading it the first time), and resolves the entry var by name
  with `ns-interns`. A newly installed version is used on the very next call.
  Reverting only moves the live pointer back.
- **Entry points**: a tool defines `(defn run [input] ...)`, where `input` is
  a keyword-keyed map, and returns a string. Hooks define
  `(defn compose-system [notes lessons] ...)`, which takes two vectors of
  strings and returns a string, and `(defn select-lessons [task lessons] ...)`,
  which takes lessons as `{:id :text :score}` maps and returns a vector of ids.
- **Isolation**: every eval and every userspace call runs in a `future` and is
  dereferenced with a timeout: 10 s for loading and tests, 30 s for calls.
  Exceptions, timeouts and wrong return types count as errors during soak.

## Run

```sh
bin/cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M] \
        [--soak N] [--no-reflect] repl | run "TASK" | refine | defs | knowledge
# or, from this directory:
clojure -M:run defs
```

`bin/cmh` runs the CLI in your current directory, and the state dir defaults
to `./.cmh`. `run` writes only the final answer to stdout. The trace (`[tool]`,
`[gate]`, `[lesson]`, `[task]` lines) goes to stderr. `run` exits 0 when the
task succeeds and 1 when it fails.

For the anthropic provider, set `ANTHROPIC_API_KEY` (sent as `x-api-key`) or
`CLAUDE_CODE_OAUTH_TOKEN` (sent as a Bearer token with the OAuth beta header
and the Claude Code system block). `ANTHROPIC_BASE_URL` overrides the default
`https://api.anthropic.com`, and plain `http://` URLs work. `CMH_MODEL` or
`--model` selects the model; the default is `claude-sonnet-5-5`.

## Test

```sh
clojure -X:test     # or: clojure -M:test
```

The suite runs offline against the mock provider and covers the 8 tests that
SPEC requires, plus seed self-tests and a few edge cases.
