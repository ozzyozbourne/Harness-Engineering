# Roc hot-reload prototype

A Zig host that embeds the Roc compiler and interpreter (`ReplSession` from
roc-lang/roc) and lets Roc definitions be replaced while it runs, the way nvim
reloads Lua. Each step in a scenario file either redefines a name or calls code:

- A definition that parses and type-checks becomes the live version of its name.
  Callers look names up at call time, so an old caller picks up the new version.
- A definition that fails is rejected with the compiler's message, and the previous
  version stays live. The session snapshots its definitions and restores them on
  failure.
- A Roc `crash` is caught and reported; the host keeps running.

## Build and run

Needs Zig 0.17.1 and a roc-lang/roc checkout. The first build downloads Roc's
prebuilt dependency bundle and takes about 7 minutes (peak RSS 1.6 GB, `-j2`).

```sh
python3 -I patch-build.py /path/to/roc host.zig   # adds a `rochost` step to roc's build.zig
cd /path/to/roc && zig build rochost -j2
./zig-out/bin/rochost /path/to/roc-proto/scenario.txt
```

Steps in the scenario are separated by `---` lines; an optional first line
`# label` is printed as the step's title.

```sh
./zig-out/bin/rochost --timeout-ms 3000 /path/to/roc-proto/work/unknowns.txt
```

With `--timeout-ms N`, each statement runs first in a forked child, which the host
kills after N ms. Roc code here is pure, so a statement that succeeded in the child
is replayed in the host's session to commit it. This costs about 0.09 s per
statement.

## Host calls: requests as data

The REPL session rejects every effectful call ("Effectful Compile Time
Expression"), even its built-in `Repl.emit!`, because it evaluates code at compile
time. So Roc code cannot call the host directly. A `# drive FN` step uses the Elm
pattern instead: `FN` returns a request such as `ReadFile("notes.txt")`, the host
checks it against its rules and performs it, then calls `FN` again with the result,
until `FN` returns `Done("...")`. The host refuses paths outside the work dir.

## Known gaps

- A well-typed definition that crashes is accepted; the crash shows up on the
  next use. A real harness needs the test-run gate the other flavors have.
- Calls whose arguments are constants are evaluated at compile time, so their
  crashes are reported as compile errors.
- The REPL evaluates through the compiler on every call, so call cost grows with
  the program.
