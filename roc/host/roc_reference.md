# Roc reference (this compiler: roc-lang/roc a5c6d8b)

Your userspace is written in current Roc. It differs from older Roc you may
remember: there is no `when`/`is` (use `match`) and no `\x ->` (use `|x|`).
Calls use parentheses and commas, `f(a, b)`, and `Num.to_str` is gone (use
`x.to_str()`). Every example below is checked against the compiler. Try any
snippet with `roc_eval` before you propose it.

## Values, numbers, strings

```roc
1 + 2  # => 3.0
7.I64 // 2  # => 3
7.I64 % 3  # => 1
3.U64.to_str()  # => "3"
3.U64.to_i64_wrap()  # => 3
I64.from_str("-12")  # => Ok(-12)
I64.from_str("x") ?? 0  # => 0
"a" == "a" and !(1 == 2)  # => True
"Hello, ${"Roc"}!"  # => "Hello, Roc!"
"cost: \$5"  # => "cost: $5"
Str.concat("ab", "cd")  # => "abcd"
"  hi ".trim()  # => "hi"
"a-b".replace_each("-", "+")  # => "a+b"
Str.split_on("a,b,c", ",")  # => ["a", "b", "c"]
Str.join_with(["a", "b"], "-")  # => "a-b"
"hello".contains("ell")  # => True
"hello".starts_with("he")  # => True
"hello".drop_prefix("he")  # => "llo"
Str.count_utf8_bytes("héllo")  # => 6
"abc".to_utf8()  # => [97, 98, 99]
Str.from_utf8([104, 105])  # => Ok("hi")
Str.from_utf8_lossy([104, 105])  # => "hi"
Str.with_ascii_uppercased("ab")  # => "AB"
"a,b".split_first(",")  # => Ok({ after: "b", before: "a" })
```

Numbers without a suffix default to `Dec` (shown as `3.0`). Write `1.I64` or
annotate the function to get integers. There is no `Str.len`; use
`Str.count_utf8_bytes` or `.to_utf8().len()`.

## Lists

```roc
[1.I64, 2, 3].map(|x| x * 2)  # => [2, 4, 6]
[1.I64, 2, 3].keep_if(|x| x > 1)  # => [2, 3]
[1.I64, 2, 3].drop_if(|x| x > 1)  # => [1]
List.fold([1.I64, 2, 3], 0, |acc, x| acc + x)  # => 6
[1.I64, 2, 3].sum()  # => 6
[1.I64, 2].len()  # => 2
[1.I64, 2].first()  # => Ok(1)
[1.I64, 2].last() ?? 0  # => 2
[1.I64, 2, 3].get(5) ?? 0  # => 0
[1.I64, 2].append(3).prepend(0)  # => [0, 1, 2, 3]
[1.I64, 2].concat([3])  # => [1, 2, 3]
[1.I64, 2, 3].contains(2)  # => True
[1.I64, 2, 3].any(|x| x > 2)  # => True
[1.I64, 2, 3].all(|x| x > 0)  # => True
[1.I64, 2, 3].count_if(|x| x > 1)  # => 2
[3.I64, 1].find_first(|x| x > 2)  # => Ok(3)
[1.I64, 2, 3].take_first(2)  # => [1, 2]
[1.I64, 2, 3].drop_first(1)  # => [2, 3]
[1.I64, 2, 3].rev()  # => [3, 2, 1]
[[1.I64], [2]].join()  # => [1, 2]
List.repeat("a", 2)  # => ["a", "a"]
["1", "x", "3"].keep_oks(I64.from_str)  # => [1, 3]
[5.I64, 2, 8].sort_with(|a, b| if a < b Before else if a > b After else Same)  # => [2, 5, 8]
```

Sort with `sort_with` and a function returning `Before`, `Same` or `After`.
Strings have no `<`; to sort them, compare `.to_utf8()` lists yourself or
avoid sorting.

## Functions, blocks, control flow

```roc
add : I64, I64 -> I64
add = |a, b| a + b

classify : I64 -> Str
classify = |n| if n < 0 "negative" else if n == 0 "zero" else "positive"

describe : List(Str) -> Str
describe = |items| match items {
    [] => "empty"
    [one] => "just ${one}"
    [first, .. as rest] => "${first} and ${rest.len().to_str()} more"
}

total : List(I64) -> I64
total = |xs| {
    var $sum = 0
    for x in xs {
        $sum = $sum + x
    }
    $sum
}

count_up : I64 -> I64
count_up = |limit| {
    var $n = 0
    while $n < limit {
        $n = $n + 2
    }
    $n
}

shout : Str -> Str
shout = |s| {
    trimmed = s.trim()
    Str.concat(trimmed, "!")
}

add(1, 2)  # => 3
classify(-4)  # => "negative"
describe(["a", "b", "c"])  # => "a and 2 more"
total([1, 2, 3])  # => 6
count_up(5)  # => 6
shout("  hey ")  # => "hey!"
"x" |> Str.concat("y")  # => "xy"
```

- A lambda is `|a, b| body`; a block `{ ... }` ends with its value.
- A mutable local is declared with `var` and its name must start with `$`.
- Boolean operators are `and`, `or` and `!`. There is no `&&` or `||`.
- `if` always needs `else`. When an `if` branch is a block that only mutates,
  write `else { {} }`.
- `match` arms are `pattern => value` with one arm per line (or arms
  separated by commas on one line).
- Every function has to be pure. There is no I/O and no `!` effects. Tools
  ask the host for I/O with requests (below).

## Records, tuples, tags, Try

```roc
Point : { x : I64, y : I64 }

norm1 : Point -> I64
norm1 = |{ x, y }| x.abs() + y.abs()

Shape : [Circle(I64), Square(I64)]

area : Shape -> I64
area = |s| match s {
    Circle(r) => 3 * r * r
    Square(side) => side * side
}

safe_div : I64, I64 -> Try(I64, [DivByZero])
safe_div = |a, b| if b == 0 Err(DivByZero) else Ok(a // b)

half_plus_one : I64 -> Try(I64, [DivByZero])
half_plus_one = |a| {
    h = safe_div(a, 2)?
    Ok(h + 1)
}

norm1({ x: -2, y: 3 })  # => 5
{ x: 1.I64, y: 2.I64 }.y  # => 2
area(Square(3))  # => 9
safe_div(7, 0)  # => Err(DivByZero)
half_plus_one(8)  # => Ok(5)
match safe_div(7, 2) { Ok(v) => v, Err(_) => 0 }  # => 3
(1.I64, "a").1  # => "a"
Dict.empty().insert("a", 1.I64).get("a")  # => Ok(1)
```

`Try(ok, err)` is Roc's result type. `x?` returns early with the `Err`, and
`x ?? default` unwraps or falls back. Type aliases are `Name : type` and
must start with an uppercase letter.

## JSON

```roc
Args : { text : Str, n : I64 }

parse_args : Str -> Try(Args, _)
parse_args = |s| Json.parse(s)

parse_list : Str -> Try(List(I64), _)
parse_list = |s| Json.parse(s)

match parse_args("{\"text\": \"hi\", \"n\": 3}") { Ok(a) => a.text, Err(_) => "bad" }  # => "hi"
parse_list("[1, 2]")  # => Ok([1, 2])
Json.to_str(["a", "b"])  # => "[\"a\",\"b\"]"
Json.to_str({ name: "x", count: 2.I64 })  # => "{\"count\":2,\"name\":\"x\"}"
```

`Json.parse` needs the target type, so annotate the function as in
`parse_args` above. Extra fields in the input are ignored. A field that is
missing makes the parse fail with `Err`, so give optional inputs their own
smaller record type or handle the `Err`.

## Userspace contracts

The source of a definition named `NAME` must define `NAME`. Every other
top-level name must start with `NAME_`, and every type must start with
`Name` in PascalCase (for `word_count`: `word_count_helper`,
`WordCountInput`). Definitions can call each other's top-level names, and
calls resolve to the live version at call time.

**Tools** have the type `NAME : Str, List(Str) -> Str` for a pure tool, or a
request union for a tool that does I/O:

```roc
LineCountInput : { path : Str }

line_count : Str, List(Str) -> [Done(Str), Fail(Str), ReadFile(Str)]
line_count = |input, results| {
    parsed : Try(LineCountInput, _)
    parsed = Json.parse(input)
    match parsed {
        Err(_) => Fail("expected {\"path\": string}")
        Ok(args) => match results {
            [] => ReadFile(args.path)
            [content, ..] =>
                if content.starts_with("ERROR: ") Fail(content)
                else Done(Str.split_on(content, "\n").keep_if(|l| !Str.is_empty(l)).len().to_str())
        }
    }
}

line_count("{\"path\": \"a.txt\"}", [])  # => ReadFile("a.txt")
line_count("{\"path\": \"a.txt\"}", ["x\ny\n"])  # => Done("2")
```

The first argument is the tool input as JSON text. The second holds the
results of the requests made so far in this call, oldest first. The host
calls the tool, performs the request it returns, and calls it again with the
result appended, until it returns `Done(text)`, `Fail(message)` or a plain
`Str`. List only the tags you use in the return type. Requests:

- `ReadFile(path)`: the UTF-8 text of a file (256 KB limit).
- `ReadBytes(path)`: the bytes of a file as decimal text `"81,76,..."`
  (128 KB limit). Decode with `Str.split_on(r, ",").keep_oks(U8.from_str)`.
- `ListDir(path)`: names, one per line; directories end with `/`.
- `WriteFile(path, content)`: returns `ok: wrote N bytes to PATH`.
- `Bash(command)`: output plus `[exit code: N]`, with a 30 s timeout and a
  20 KB cap.
- `Done(text)` ends the call successfully; `Fail(message)` ends it as an
  error.

Paths are relative to the work dir. A path outside the work dir, or a write
to the state dir or the kernel, is refused, and the result is
`ERROR: refused: ...`. Every failed request returns a result that starts with
`ERROR: `, so check for it. A call may make at most 24 requests.

Tool tests can script request results:
`{"input": {...}, "responses": ["file text"], "expect": "2", "expect_requests": ["ReadFile(\"a.txt\")"]}`.
Without `responses`, tests perform ReadFile/ReadBytes/ListDir for real and
refuse WriteFile/Bash.

**Hooks:** `compose_system : List(Str), List(Str) -> Str` takes notes and
lessons and returns the supplemental prompt.
`select_lessons : Str, List({ id : I64, text : Str, score : I64 }) -> List(I64)`
takes the task and the lessons and returns lesson ids.

Every call runs in a separate child process with a 10 s limit. A crash, an
infinite loop or a stack overflow only ends that child, and you get a
structured report back.
