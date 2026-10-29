# cmh, Forth flavor

The fifth implementation of [SPEC.md](../SPEC.md). The kernel is a Rust host
(`host/`, binary `cmh`). The agent's userspace is written in our own small,
late-bound Forth (`vm/`). The Forth interpreter is Rust compiled to
`wasm32-unknown-unknown` and runs inside Wasmtime. Nothing the agent writes
can crash or hang the host, and every failure comes back to the model as a
precise, structured report.

## The wall

- **WebAssembly sandbox.** The VM only sees its own linear memory and a few
  host imports: `json-get`, `json-len`, `json-str`, `file-read`,
  `file-write` and `remember`. File access is limited to the working
  directory and refuses `..`, the state dir, `forth/host` and the base
  prompt. The VM has no network access.
- **Fuel.** `consume_fuel` gives each entry into the VM (a call or a compile)
  100M instructions by default. Infinite loops trap with `fuel-exhausted`.
- **Wall-clock backstop.** Epoch interruption ends any entry after 5 s
  (`timeout`).
- **Memory cap.** `StoreLimits`-style `ResourceLimiter`: linear memory may
  not grow past 64 MB (`memory-limit`).
- **Traps become data.** Wasmtime traps (out of bounds, unreachable, stack
  overflow, out of fuel) are caught in `host/src/kernel/forth.rs`. The host
  then asks the trapped VM where it was (`cmh_where`), so a trap is reported
  like any Forth error.
- **Snapshot rollback.** The whole Forth image (dictionary, slots, code, data
  space, stacks) lives in linear memory. Before each call the host copies
  memory. After a trap, or after any error from a trial version, it builds a
  fresh instance with that copy written back, then reverts the trial per
  SPEC.
- **Scratchpad.** The `forth_eval` kernel tool compiles and runs a source
  in a throwaway instance and returns the output or the structured error.
  Nothing is journaled, so the model can debug without spending proposals.
- **Scratch validation.** `propose_def` compiles the source and runs its
  tests in a brand-new instance with its own store and fuel. Only then is it
  compiled into the live VM as a `trial`.

## Self-modification inside the VM

- **Late-bound slots.** Every compiled call goes through a named
  indirection cell. Each definition gets a private namespace for its helper
  words and a public slot named after the definition. Redefining
  `word_count` rebinds its slot, so an existing caller such as
  `: run word_count s" !" concat ;` uses the new version on its next call.
  Reverting or rolling back points the slot back.
- **Versioned words.** The host journal keeps name, version, source, tests
  and description for every definition. The VM records which (def, version)
  each word belongs to, so errors name both.
- **Lazy loading.** At startup only the core words and the three seed
  definitions are compiled. Any other kept or trial definition is compiled
  from the journal the first time it is called. When a call hits an unbound
  public slot, the VM suspends with `NEED_LOAD`; the host compiles that
  definition and resumes the VM. Names referenced while compiling are
  checked against the live registry.
- **Structured errors.** Reports have this shape:
  `forth error [kind] in word W of DEF vN, line L, at TOKEN: message`,
  followed by the data stack, the call trace, the top two items read as a
  string, console output, the input and a hint. Error kinds:
  `stack-underflow`, `stack-overflow`, `return-stack-overflow`,
  `undefined-word` (with a "did you mean"), `division-by-zero`,
  `bad-address`, `compile-error`, `contract`, `host-error`,
  `fuel-exhausted`, `timeout`, `memory-limit`, `wasm-out-of-bounds`,
  `wasm-stack-overflow` and `vm-panic`.

## The dialect

The full reference is
[`host/src/kernel/forth_reference.md`](host/src/kernel/forth_reference.md).
The kernel sends it to the model as a system block right after the immutable
`base-prompt.md`. In short:

- 64-bit integer cells, case-insensitive words, `( addr len )` strings.
- Stack: `dup drop swap over rot -rot nip tuck 2dup 2drop 2swap 2over pick depth >r r> r@`
- Arithmetic and logic: `+ - * / mod /mod negate abs min max 1+ 1- = <> < > <= >= 0= 0< 0> and or xor invert true false within`
- Memory: `@ ! c@ c! +! here allot , c, cells variable constant create`
- Control: `: ; if else then begin until again while repeat do ?do loop +loop i j leave exit recurse`
- Strings: `s" s\" ." char [char] bl compare s= index concat /string lower trim n>s s>n`
- String builder: `<b b+ bc+ bn+ b>`
- Debug output: `type emit cr . .s` (shown in error reports)
- Host words: `json-get json-len json-str file-read file-write remember`

**Contract.** Every tool and hook source defines
`: run ( json-addr json-len -- out-addr out-len ) ... ;`.

- A tool receives its input object.
- `compose_system` receives `{"notes": [...], "lessons": [...]}` and returns
  the supplemental prompt.
- `select_lessons` receives `{"task", "lessons": [{id,text,score}]}` and
  returns a JSON id array such as `[3,1]`.

The seeds are in [`seed/`](seed/): `word_count.fs`, `compose_system.fs` and
`select_lessons.fs`.

## Build, test, run

```sh
cd forth
cargo build --release      # host/build.rs builds vm/ for wasm32 and embeds it
cargo test                 # 8 SPEC tests + Forth wall tests, offline (mock provider)
./target/release/cmh run "reverse the string 'stressed' with a userspace tool"
./target/release/cmh defs
./target/release/cmh knowledge
./target/release/cmh repl
```

Requires the `wasm32-unknown-unknown` target
(`rustup target add wasm32-unknown-unknown`). `.cargo/config.toml` limits
builds to 2 jobs. The CLI, providers (`anthropic` and
`mock --script FILE`), `ANTHROPIC_BASE_URL`, journal format and exit codes
are the same as in the other flavors. The state dir defaults to `.cmh/`.
