"""Offline test suite for cmh (Roc flavor): drives the real binary through the
mock provider. Covers the 8 SPEC tests plus the Roc-specific cases.

Usage: python3 -I roc/tests/run_tests.py [-k SUBSTRING] [--bin PATH]
"""
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).resolve().parent
BIN = str(HERE.parent / "bin" / "cmh")
BASE_PROMPT = (HERE.parent.parent / "base-prompt.md").read_text()

WC_V2 = '''WordCountInput : { text : Str }

word_count : Str, List(Str) -> Str
word_count = |input, _results| {
    parsed : Try(WordCountInput, _)
    parsed = Json.parse(input)
    match parsed {
        Ok(args) => Str.concat("words: ", Str.split_on(args.text, " ").keep_if(|w| !Str.is_empty(w)).len().to_str())
        Err(_) => "bad input"
    }
}
'''

WC_CRASHY = '''WordCountInput : { text : Str }

word_count : Str, List(Str) -> Str
word_count = |input, _results| {
    parsed : Try(WordCountInput, _)
    parsed = Json.parse(input)
    match parsed {
        Ok(args) => if args.text == "boom" crash "boom input" else Str.split_on(args.text, " ").keep_if(|w| !Str.is_empty(w)).len().to_str()
        Err(_) => "bad input"
    }
}
'''

WC_TYPE_ERROR = '''word_count : Str, List(Str) -> Str
word_count = |input, _results| Str.concat(input, 1)
'''

SHOUT = '''shout_words : Str, List(Str) -> Str
shout_words = |input, results| Str.concat(word_count(input, results), "!")
'''

SNEAK = '''sneak : Str, List(Str) -> [Done(Str), ReadFile(Str)]
sneak = |_input, results| match results {
    [] => ReadFile("../secret.txt")
    [r, ..] => Done(r)
}
'''

LINE_COUNT = '''LineCountInput : { path : Str }

line_count : Str, List(Str) -> [Done(Str), Fail(Str), ReadFile(Str)]
line_count = |input, results| {
    parsed : Try(LineCountInput, _)
    parsed = Json.parse(input)
    match parsed {
        Err(_) => Fail("expected {\\"path\\": string}")
        Ok(args) => match results {
            [] => ReadFile(args.path)
            [content, ..] =>
                if content.starts_with("ERROR: ") Fail(content)
                else Done(Str.split_on(content, "\\n").keep_if(|l| !Str.is_empty(l)).len().to_str())
        }
    }
}
'''

SPIN = '''spin_input : Str, List(Str) -> Str
spin_input = |input, _results| if input.contains("loop") spin_input_go(0) else "fine"

spin_input_go : U64 -> Str
spin_input_go = |n| spin_input_go(n + 1)
'''

COMPOSE_V2 = '''compose_system : List(Str), List(Str) -> Str
compose_system = |notes, lessons| Str.concat("CUSTOM-HOOK-MARK\\n", Str.join_with(notes.concat(lessons), "\\n"))
'''

REVERSE = '''ReverseWordsInput : { text : Str }

reverse_words : Str, List(Str) -> Str
reverse_words = |input, _results| {
    parsed : Try(ReverseWordsInput, _)
    parsed = Json.parse(input)
    match parsed {
        Ok(args) => Str.join_with(Str.split_on(args.text, " ").rev(), " ")
        Err(_) => "bad input"
    }
}
'''

TEXT_SCHEMA = {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}


def tool_use(name, inp, i=[0]):
    i[0] += 1
    return {"type": "tool_use", "id": f"t{i[0]}", "name": name, "input": inp}


def text(t):
    return [{"type": "text", "text": t}]


def propose(name, source, tests, kind="tool", schema=TEXT_SCHEMA, desc="test def"):
    inp = {"name": name, "kind": kind, "source": source, "description": desc, "tests": tests}
    if kind == "tool":
        inp["input_schema"] = schema
    return tool_use("propose_def", inp)


class Env:
    def __init__(self, root):
        self.root = pathlib.Path(root)
        self.work = self.root / "work"
        self.state = self.root / "state"
        self.work.mkdir()
        self.n = 0

    def run(self, script, *args, reflect=False, env=None, timeout=600):
        self.n += 1
        sp = self.root / f"script{self.n}.json"
        sp.write_text(json.dumps(script))
        cmd = [BIN, "--state", str(self.state), "--provider", "mock", "--script", str(sp)]
        if not reflect:
            cmd.append("--no-reflect")
        e = dict(os.environ)
        e.update(env or {})
        p = subprocess.run(cmd + list(args), cwd=self.work, capture_output=True, text=True, timeout=timeout, env=e)
        return p.returncode, p.stdout, p.stderr

    def cli(self, *args):
        p = subprocess.run([BIN, "--state", str(self.state), *args], cwd=self.work, capture_output=True, text=True, timeout=600)
        return p.returncode, p.stdout, p.stderr

    def requests(self):
        log = self.state / "mock-requests.jsonl"
        return [json.loads(l) for l in log.read_text().splitlines() if l.strip()]

    def tool_results(self):
        """tool_result blocks of the last agent request (not reflection)."""
        reqs = [r for r in self.requests() if "tools" in r]
        out = []
        for m in reqs[-1]["messages"]:
            if isinstance(m["content"], list):
                out += [b for b in m["content"] if b.get("type") == "tool_result"]
        return out

    def journal(self):
        return [json.loads(l) for l in (self.state / "journal.jsonl").read_text().splitlines() if l.strip()]

    def defs(self):
        return self.cli("defs")[1]


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)


def live_row(defs, name):
    for line in defs.splitlines():
        if line.split() and line.split()[0] == name:
            return line
    return ""


# ------------------------------------------------------------------ tests

def test_1_late_binding(e):
    """propose word_count v2; the next call in the same process uses it, and an
    existing caller (shout_words) picks up v3 without being redefined."""
    script = [
        [propose("word_count", WC_V2, [{"input": {"text": "a b c"}, "expect": "words: 3"}])],
        [tool_use("word_count", {"text": "one two"})],
        [propose("shout_words", SHOUT, [{"input": {"text": "a b"}, "expect": "!"}])],
        [tool_use("shout_words", {"text": "x y z"})],
        [propose("word_count", WC_V2.replace('"words: "', '"count="'), [{"input": {"text": "a"}, "expect": "count=1"}])],
        [tool_use("shout_words", {"text": "x y z"})],
        text("done"),
    ]
    code, out, err = e.run(script, "run", "late binding")
    r = [b["content"] for b in e.tool_results()]
    check(code == 0, f"exit {code}\n{err}")
    check(r[0].startswith("trial v2"), r[0])
    check(r[1] == "words: 2", r[1])
    check(r[3] == "words: 3!", r[3])
    check(r[4].startswith("trial v3"), r[4])
    check(r[5] == "count=3!", f"caller did not late-bind: {r[5]}")


def test_2_gate_rejection(e):
    """a type error (with its location) and a failing test are both rejected;
    the live version is unchanged."""
    script = [
        [propose("word_count", WC_TYPE_ERROR, [{"input": {"text": "a"}, "expect": "1"}])],
        [propose("word_count", WC_V2, [{"input": {"text": "a b"}, "expect": "nope"}])],
        [propose("word_count", "word_count = |input, _r| Str.concat(input", [{"input": {"text": "a"}, "expect": "1"}])],
        [tool_use("word_count", {"text": "a b"})],
        text("done"),
    ]
    code, out, err = e.run(script, "run", "gate")
    r = [b["content"] for b in e.tool_results()]
    check(r[0].startswith("error: rejected: roc error [type-error] in word_count v2 (tool), line 2"), r[0])
    check("Type Mismatch" in r[0], r[0])
    check("wrong output" in r[1] and "expected the output to contain \"nope\"" in r[1], r[1])
    check("[parse-error]" in r[2], r[2])
    check(r[3] == "2", f"live version changed: {r[3]}")
    check("v1   kept" in live_row(e.defs(), "word_count"), e.defs())
    rejected = [j for j in e.journal() if j["type"] == "def_rejected"]
    check(len(rejected) == 3, rejected)


def test_3_soak_revert(e):
    """a trial that crashes on one input reverts on that use; v1 is live again
    and a verdict exists."""
    script = [
        [propose("word_count", WC_CRASHY, [{"input": {"text": "a b"}, "expect": "2"}])],
        [tool_use("word_count", {"text": "boom"})],
        [tool_use("word_count", {"text": "boom"})],
        text("done"),
    ]
    e.run(script, "run", "revert")
    r = [b["content"] for b in e.tool_results()]
    check("roc error [crash] in word_count v2" in r[1] and "boom input" in r[1], r[1])
    check("was reverted by this error (live: v1)" in r[1], r[1])
    check(r[2] == "1", f"v1 not live again: {r[2]}")
    check("v1   kept" in live_row(e.defs(), "word_count"), e.defs())
    k = e.cli("knowledge")[1]
    check("def word_count v2" in k and "reverted" in k and "boom" in k, k)


def test_4_soak_keep(e):
    script = [
        [propose("word_count", WC_V2, [{"input": {"text": "a b"}, "expect": "words: 2"}])],
        [tool_use("word_count", {"text": "a"})],
        [tool_use("word_count", {"text": "a b"})],
        text("done"),
    ]
    code, _, err = e.run(script, "--soak", "2", "run", "keep")
    check(code == 0, err)
    check("v2   kept" in live_row(e.defs(), "word_count"), e.defs())
    check("kept after 2 clean uses" in e.cli("knowledge")[1], "no keep verdict")


def test_5_persistence(e):
    """a fresh process on the same state dir sees the same registry, soak
    counters and knowledge."""
    script = [
        [propose("word_count", WC_V2, [{"input": {"text": "a b"}, "expect": "words: 2"}])],
        [tool_use("word_count", {"text": "a"})],
        [tool_use("remember", {"text": "persist this lesson"})],
        [propose("word_count", WC_TYPE_ERROR, [{"input": {"text": "a"}, "expect": "1"}])],
        text("done"),
    ]
    e.run(script, "run", "persistence")
    d1, k1 = e.defs(), e.cli("knowledge")[1]
    check("v2   trial (1/3 clean uses)" in live_row(d1, "word_count"), d1)
    check("persist this lesson" in k1, k1)
    # The next process continues the soak where the last one stopped.
    e.run([[tool_use("word_count", {"text": "a"})], [tool_use("word_count", {"text": "a"})], text("ok")], "run", "persist again")
    d2 = e.defs()
    check("v2   kept" in live_row(d2, "word_count"), d2)
    check(d2 == e.defs() and e.cli("knowledge")[1] == e.cli("knowledge")[1], "replay is not stable")


def test_6_hook_redefinition(e):
    script = [
        [propose("compose_system", COMPOSE_V2, [{"input": {"notes": [], "lessons": ["x"]}, "expect": "CUSTOM-HOOK-MARK"}], kind="hook")],
        text("done"),
    ]
    code, _, err = e.run(script, "run", "hook")
    check(code == 0, err)
    reqs = [r for r in e.requests() if "tools" in r]
    first, second = reqs[0]["system"], reqs[1]["system"]
    check(all("CUSTOM-HOOK-MARK" not in b["text"] for b in first), "marker too early")
    check(first[0]["text"] == BASE_PROMPT, "base prompt not first")
    check(second[0]["text"] == BASE_PROMPT, "base prompt dropped by the hook")
    check(any(b["text"].startswith("CUSTOM-HOOK-MARK") for b in second), json.dumps(second)[:500])


def test_7_lesson_scoring(e):
    e.run([[tool_use("remember", {"text": "always check files first"})], text("ok")], "run", "learn")
    ok = [text("fine")]
    bad = [[tool_use("no_such_tool", {})], text("gave up")]
    code, _, _ = e.run(ok, "run", "check files task")
    check(code == 0, "success task failed")
    scores = []
    for _ in range(4):
        code, _, _ = e.run(bad, "run", "check files task")
        check(code == 1, "failing task exited 0")
        scores.append(re.search(r"score\s+(-?\d+)", e.cli("knowledge")[1]).group(1))
    check(scores == ["0", "-1", "-2", "-3"], scores)
    k = e.cli("knowledge")[1]
    check("[retired]" in k, k)
    # Retired lessons are no longer selected.
    e.run(ok, "run", "check files task")
    last = [r for r in e.requests() if "tools" in r][-1]
    check(all("always check files" not in b["text"] for b in last["system"]), "retired lesson still in prompt")


def test_8_end_to_end_run(e):
    """`run`: the model proposes a tool and calls it in the same task; stdout
    holds only the final answer; reflection adds a lesson."""
    script = [
        [propose("reverse_words", REVERSE, [{"input": {"text": "a b c"}, "expect": "c b a"}])],
        [tool_use("reverse_words", {"text": "one two three"})],
        text("The reversed text is: three two one"),
        text('["Use reverse_words to reverse word order."]'),
    ]
    code, out, err = e.run(script, "run", "reverse the words in 'one two three'", reflect=True)
    check(code == 0, err)
    check(out == "The reversed text is: three two one\n", repr(out))
    check("[tool] propose_def reverse_words -> trial v1" in err, err)
    check(e.tool_results()[1]["content"] == "three two one", e.tool_results())
    check("Use reverse_words" in e.cli("knowledge")[1], "reflection lesson missing")


def test_hang_killed(e):
    """a definition that hangs on one input is killed at the deadline; the host
    survives, the trial is removed, and the next call still works."""
    script = [
        [propose("spin_input", SPIN, [{"input": {"text": "hello"}, "expect": "fine"}])],
        [tool_use("spin_input", {"text": "loop"})],
        [tool_use("word_count", {"text": "still alive"})],
        text("done"),
    ]
    t0 = time.time()
    e.run(script, "run", "hang", env={"CMH_ROC_TIMEOUT_MS": "3000"})
    took = time.time() - t0
    r = [b["content"] for b in e.tool_results()]
    check(r[0].startswith("trial v1"), r[0])
    check("roc error [timeout] in spin_input v1" in r[1] and "child was killed" in r[1], r[1])
    check("definition removed" in r[1], r[1])
    check(r[2] == "2", r[2])
    check("removed" in live_row(e.defs(), "spin_input"), e.defs())
    check(took < 120, f"took {took:.0f}s")


def test_refused_request(e):
    """a request for a path outside the work dir is refused; the tool sees the
    refusal and the file is never read. A request inside the work dir works."""
    (e.root / "secret.txt").write_text("TOP-SECRET")
    (e.work / "notes.txt").write_text("a\nb\nc\n")
    script = [
        [propose("sneak", SNEAK, [{"input": {}, "responses": ["x"], "expect": "x", "expect_requests": ["ReadFile(\"../secret.txt\")"]}], schema={"type": "object"})],
        [tool_use("sneak", {})],
        [propose("line_count", LINE_COUNT, [{"input": {"path": "a.txt"}, "responses": ["x\ny\n"], "expect": "2"}],
                 schema={"type": "object", "properties": {"path": {"type": "string"}}})],
        [tool_use("line_count", {"path": "notes.txt"})],
        [tool_use("line_count", {"path": "/etc/passwd"})],
        text("done"),
    ]
    _, _, err = e.run(script, "run", "refuse")
    r = [b["content"] for b in e.tool_results()]
    check(r[1].startswith("ERROR: refused: ../secret.txt") and "outside the work dir" in r[1], r[1])
    check("TOP-SECRET" not in r[1], r[1])
    check(r[3] == "3", r[3])
    check("roc error [fail] in line_count v1" in r[4] and "refused" in r[4], r[4])
    check("[request] sneak ReadFile(\"../secret.txt\") -> ERROR: refused" in err, err)


def test_scratchpad(e):
    script = [
        [tool_use("roc_eval", {"source": "double : I64 -> I64\ndouble = |x| x * 2", "expr": "double(21)"})],
        [tool_use("roc_eval", {"expr": "word_count(\"{\\\"text\\\": \\\"a b\\\"}\", [])"})],
        [tool_use("roc_eval", {"expr": "Str.concat(\"a\", 1)"})],
        [tool_use("roc_eval", {"source": "f = |x| (x", "expr": "f(1)"})],
        text("done"),
    ]
    e.run(script, "run", "scratch")
    r = [b["content"] for b in e.tool_results()]
    check(r[0] == "value: 42", r[0])
    check(r[1] == 'value: "2"', r[1])
    check("[type-error]" in r[2], r[2])
    check("[parse-error]" in r[3], r[3])
    check(not any(j["type"].startswith("def_") and j.get("name") == "roc_eval" for j in e.journal()), "scratchpad journaled")


def test_seeds(e):
    code, out, err = e.run([], "check-seeds")
    check(code == 0, out + err)
    check(out.count("ok   ") == 3, out)


def test_reference_examples(e):
    code, out, _ = e.cli("check-reference")
    check(code == 0 and "examples ok" in out, out)


def test_usage_errors(e):
    p = subprocess.run([BIN, "frobnicate"], capture_output=True, text=True)
    check(p.returncode == 2, p.returncode)
    p = subprocess.run([BIN, "--provider", "mock", "run", "x"], capture_output=True, text=True, cwd=e.work)
    check(p.returncode == 2, p.returncode)


def main():
    global BIN
    args = sys.argv[1:]
    only = None
    if "-k" in args:
        only = args[args.index("-k") + 1]
    if "--bin" in args:
        BIN = args[args.index("--bin") + 1]
    tests = [(n, f) for n, f in globals().items() if n.startswith("test_") and (only is None or only in n)]
    failed = 0
    for name, fn in tests:
        root = tempfile.mkdtemp(prefix="cmh-roc-test-")
        t0 = time.time()
        try:
            fn(Env(root))
            print(f"PASS {name} ({time.time() - t0:.1f}s)", flush=True)
            shutil.rmtree(root)
        except Exception as ex:  # noqa: BLE001
            failed += 1
            print(f"FAIL {name} ({time.time() - t0:.1f}s): {ex}\n  state kept in {root}", flush=True)
    print(f"{len(tests) - failed}/{len(tests)} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
