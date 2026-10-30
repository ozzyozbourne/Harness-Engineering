# cmh Forth reference (kernel-owned)

Userspace tools and hooks are written in cmh Forth, a small late-bound Forth
that runs inside the kernel image (a WebAssembly component) with a step
budget and a fixed data space; memory accesses outside that data space are
refused. Nothing you write can crash the harness;
every error comes back to you as a structured report (kind, word, definition
and version, line, token, data stack, call trace, input) with a hint.

## Contract
Every tool or hook source must define

    : run ( json-addr json-len -- out-addr out-len ) ... ;

`run` receives the input JSON object as a string and must leave exactly one
string. Define helper words before `run` in the same source; they are private
to that definition. Another live userspace definition can be called by its
name: it runs that definition's `run` (late bound, compiled on first use).
- tool: input is the tool input object (shaped by your input_schema).
- hook `compose_system`: input `{"notes": [string], "lessons": [string]}`;
  output is the supplemental system prompt text.
- hook `select_lessons`: input `{"task": string, "lessons": [{"id","text","score"}]}`;
  output is a JSON array of ids, e.g. `[3,1]`.

## Language
Integers are 64-bit cells; no floating point. Words are case-insensitive.
Flags: true is -1, false is 0. Comments: `\ to end of line` and `( ... )`.
Strings are `( addr len )` pairs. String results from words like `concat`,
`n>s`, `b>`, `json-get` live in per-call scratch space (valid until `run`
returns); `s"` literals and `variable`/`create` data are permanent.

- Defining: `: name ... ;`  `variable v`  `10 constant c`  `create buf 64 allot`
  `recurse`  `exit`
- Control (inside `:` only): `if ... else ... then`  `begin ... until`
  `begin ... again`  `begin ... while ... repeat`  `limit start do ... loop`
  `?do` (skips when start >= limit)  `+loop`  `i`  `j`  `leave`
- Stack: `dup drop swap over rot -rot nip tuck 2dup 2drop 2swap 2over pick depth >r r> r@`
- Arithmetic: `+ - * / mod /mod negate abs min max 1+ 1-`
- Compare/logic: `= <> < > <= >= 0= 0< 0> and or xor invert true false within ( n lo hi -- f )`
- Memory: `@ ! c@ c! +! here allot , c, cells` (a cell is 8 bytes)
- Literals: `123`  `-5`  `char x` / `[char] x`  `s" text"`  `s\" esc\n\t\"\\"`  `." print"`  `bl`
- Strings: `compare ( a1 u1 a2 u2 -- -1|0|1 )`  `s= ( a1 u1 a2 u2 -- f )`
  `index ( a1 u1 a2 u2 -- pos|-1 )` (find 2 in 1)  `concat ( a1 u1 a2 u2 -- a u )`
  `/string ( a u n -- a+n u-n )`  `lower ( a u -- a u )`  `trim ( a u -- a u )`
  `n>s ( n -- a u )`  `s>n ( a u -- n flag )`
- String builder (nestable): `<b` start, `b+ ( a u -- )` append string,
  `bc+ ( c -- )` append char, `bn+ ( n -- )` append number, `b> ( -- a u )` finish
- Debug output (shown in error reports): `type emit cr . .s`

## Host words
- `json-get ( json-a json-u path-a path-u -- a u )`: value at a dot path such as
  `text`, `items.0.name` ("" is the root). Strings come back unquoted, other
  values as JSON text. Missing keys are an error; test with `json-len` first.
- `json-len ( json-a json-u path-a path-u -- n )`: array/object size or string
  length; 0 for numbers/bools/null; -1 if the path is missing.
- `json-str ( a u -- a2 u2 )`: the string as a quoted, escaped JSON string.
- `file-read ( path-a path-u -- a u )`, `file-write ( a u path-a path-u -- )`:
  only inside the working directory (no `..`, not the state dir or kernel).
- `remember ( a u -- )`: record a lesson.
No network access exists inside the VM.

## Example tool
    \ {"text": string} -> the text reversed
    : reverse-str ( a u -- a2 u2 )
      <b  dup 0 ?do  2dup + i - 1- c@ bc+  loop  2drop  b> ;
    : run ( json-a json-u -- a u )  s" text" json-get reverse-str ;

## Trying code first
Use the `forth_eval` tool as a scratchpad: it compiles and runs a source with
a `run` word in a throwaway VM and returns the output or the error report.
Nothing is proposed or recorded, so debug there and call `propose_def` only
with code that already works.
