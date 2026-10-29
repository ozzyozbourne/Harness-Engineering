import Cmh
/-! The 8 required tests (mock provider, offline). Run with `lake exe cmh-test`. -/
open Lean Cmh

initialize failures : IO.Ref (Array String) ← IO.mkRef #[]
initialize checks : IO.Ref Nat ← IO.mkRef 0

def check (label : String) (cond : Bool) (detail : String := "") : IO Unit := do
  checks.modify (· + 1)
  unless cond do
    failures.modify (·.push s!"{label} {detail}")
    IO.eprintln s!"    FAIL: {label} {detail}"

def freshDir (tag : String) : IO System.FilePath := do
  let d : System.FilePath := s!"/tmp/cmh-test-{← IO.Process.getPID}-{tag}"
  if ← d.pathExists then IO.FS.removeDirAll d
  return d

def mkKernel (dir : System.FilePath) (script : Array Json := #[]) (soak := 3) : IO Kernel := do
  Kernel.open { stateDir := dir, soak, reflect := false, trace := false } (← mockProvider script)

def parseJ (s : String) : Json := (Json.parse s).toOption.getD Json.null

def text (s : String) : Json := Json.arr #[Json.mkObj [("type", "text"), ("text", Json.str s)]]

def liveOf (k : Kernel) (n : String) : IO (Option Def) := return (← k.st).liveDef? n

/-- (version, status) of the live version of `n`. -/
def liveState (k : Kernel) (n : String) : IO (Option (Nat × String)) :=
  return (← liveOf k n).map fun d => (d.version, d.status)

def lessonById (k : Kernel) (id : Nat) : IO (Option Entry) :=
  return (← k.st).knowledge.find? (·.id == id)

def wcV2 : String := "
def run (input : Lean.Json) : IO String := do
  let t := (input.getObjValAs? String \"text\").toOption.getD \"\"
  pure (\"words=\" ++ toString ((t.splitToList Char.isWhitespace).filter (· ≠ \"\")).length)
theorem two_eq : 1 + 1 = 2 := rfl
"

def wcTests : Json := parseJ "[{\"input\": {\"text\": \"a b\"}, \"expect\": \"words=2\"}]"

def propose (k : Kernel) (n kind src : String) (tests : Json) : IO (String × Bool) :=
  k.propose n kind src s!"{n} test version" tests
    (parseJ "{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}}}")

/-- 1. Late binding: a new version is used by the very next call in the same process. -/
def testLateBinding : IO Unit := do
  let k ← mkKernel (← freshDir "late")
  for d in (← k.st).defs do  -- the shipped seeds pass their own gate
    let r ← k.validate d.name d.kind 99 d.source d.tests
    check s!"seed {d.name} validates" (r matches .ok _) (match r with | .error e => e | _ => "")
  let (o1, _) ← k.callTool "word_count" (parseJ "{\"text\": \"x y z\"}")
  check "v1 output" (o1 == "3") o1
  let (msg, ok) ← propose k "word_count" "tool" wcV2 wcTests
  check "v2 accepted" ok msg
  let (o2, err) ← k.callTool "word_count" (parseJ "{\"text\": \"x y z\"}")
  check "next call uses v2" (o2 == "words=3" && !err) o2
  check "v2 is live trial" ((← liveState k "word_count") == some (2, "trial"))

/-- 2. Gate rejection: compile errors, failing tests, sorry and false theorems are rejected. -/
def testGateRejection : IO Unit := do
  let k ← mkKernel (← freshDir "reject")
  let cases : List (String × String × Json) := [
    ("syntax", "def run (input : Lean.Json) : IO String := pure (1 + \"x\")", wcTests),
    ("failing test", "def run (input : Lean.Json) : IO String := pure \"words=0\"", wcTests),
    ("wrong shape", "def run (n : Nat) : String := toString n", wcTests),
    ("sorry", wcV2 ++ "theorem t : 2 = 3 := sorry", wcTests),
    ("false theorem", wcV2 ++ "theorem t : 2 = 3 := rfl", wcTests),
    ("no tests", wcV2, parseJ "[]")]
  for (label, src, tests) in cases do
    let (msg, ok) ← propose k "word_count" "tool" src tests
    check s!"{label} rejected" (!ok && msg.startsWith "rejected") msg
  check "live version unchanged" ((← liveState k "word_count") == some (1, "kept"))
  let (o, _) ← k.callTool "word_count" (parseJ "{\"text\": \"x y\"}")
  check "v1 still answers" (o == "2") o
  let (msg, _) ← propose k "word_count" "tool" "def run (input : Lean.Json) : IO String := pure (1 + \"x\")" wcTests
  check "elaborator message returned" (msg.contains "line 1") msg

/-- 3. Soak revert: a version that throws reverts on that use; a verdict is recorded. -/
def testSoakRevert : IO Unit := do
  let k ← mkKernel (← freshDir "revert")
  let src := "
def run (input : Lean.Json) : IO String := do
  let t := (input.getObjValAs? String \"text\").toOption.getD \"\"
  if t == \"boom\" then throw (IO.userError \"exploded on boom\")
  pure (\"words=\" ++ toString (t.splitOn \" \").length)
"
  let (_, ok) ← propose k "word_count" "tool" src wcTests
  check "throwing version accepted" ok
  let (_, err1) ← k.callTool "word_count" (parseJ "{\"text\": \"a b\"}")
  check "clean use ok" (!err1)
  let (o, err) ← k.callTool "word_count" (parseJ "{\"text\": \"boom\"}")
  check "error returned to model" (err && o.contains "exploded") o
  check "previous version live again" ((← liveState k "word_count") == some (1, "kept"))
  check "v2 marked reverted" ((((← k.st).find? "word_count" 2).map (·.status)) == some "reverted")
  let vs := (← k.st).verdicts
  check "verdict entry exists" (vs.any fun v => v.text.contains "word_count v2" && v.text.contains "reverted"
    && v.text.contains "exploded") (toString (vs.map (·.text)))
  -- a proposal without tests is rejected
  let flaky := "def run (input : Lean.Json) : IO String := throw (IO.userError \"no\")"
  let (_, ok2) ← k.propose "flaky" "tool" flaky "always fails" (parseJ "[]") Json.null
  check "flaky without tests rejected" (!ok2)

/-- 4. Soak keep: after `soak` clean uses the version is kept. -/
def testSoakKeep : IO Unit := do
  let k ← mkKernel (← freshDir "keep") (soak := 3)
  let (_, ok) ← propose k "word_count" "tool" wcV2 wcTests
  check "accepted" ok
  for i in [0:3] do
    let d ← liveOf k "word_count"
    check s!"trial before use {i + 1}" ((d.map (·.status)) == some "trial")
    discard <| k.callTool "word_count" (parseJ "{\"text\": \"one two\"}")
  check "kept after 3 clean uses" ((← liveState k "word_count") == some (2, "kept"))
  check "keep verdict" ((← k.st).verdicts.any fun v => v.text.contains "word_count v2" && v.text.contains "kept")

/-- 5. Persistence: a fresh kernel on the same state dir has identical state. -/
def testPersistence : IO Unit := do
  let dir ← freshDir "persist"
  let k ← mkKernel dir #[text "done"]
  discard <| propose k "word_count" "tool" wcV2 wcTests
  discard <| k.callTool "word_count" (parseJ "{\"text\": \"one\"}")   -- soak 1/3
  discard <| propose k "word_count" "tool" "def run (input : Lean.Json) : IO String := pure \"bad\"" wcTests
  discard <| k.remember "Prefer small composable tools"
  discard <| k.runTask "write a small tool"
  discard <| k.propose "style" "note" "Answer tersely." "terse answers" Json.null Json.null
  let before := (← k.st).snapshot.compress
  let k2 ← mkKernel dir
  let after := (← k2.st).snapshot.compress
  check "replayed state identical" (before == after) s!"\n{before}\n{after}"
  let d ← liveOf k2 "word_count"
  check "soak counter persisted" ((d.map fun d => (d.version, d.status, d.soakUses)) == some (2, "trial", 1))
  let (o, _) ← k2.callTool "word_count" (parseJ "{\"text\": \"a b c\"}")
  check "replayed version callable" (o == "words=3") o
  check "lesson persisted with score" ((← k2.st).lessons.any fun e => e.text == "Prefer small composable tools" && e.score == 1)

/-- 6. Hook redefinition changes the prompt seen by the provider; the base prompt stays. -/
def testHookRedefinition : IO Unit := do
  let k ← mkKernel (← freshDir "hook") #[text "ok"]
  let src := "
def composeSystem (notes lessons : List String) : String :=
  \"CUSTOM-SUPPLEMENT notes=\" ++ toString notes.length ++ \" lessons=\" ++ toString lessons.length
"
  let (msg, ok) ← k.propose "compose_system" "hook" src "custom" (parseJ
    "[{\"input\": {\"notes\": [\"n\"], \"lessons\": []}, \"expect\": \"CUSTOM-SUPPLEMENT notes=1\"}]") Json.null
  check "hook accepted" ok msg
  mockRequests.set #[]
  discard <| k.runTask "hello"
  let reqs ← mockRequests.get
  let sys := (jArr (reqs[0]?.getD Json.null) "system").toList.filterMap (·.getStr?.toOption)
  check "custom supplement in system prompt" (sys.any (·.contains "CUSTOM-SUPPLEMENT")) (toString sys)
  check "base prompt still first" (sys.headD "" == k.basePrompt && k.basePrompt.contains "You are cmh")
  check "hook use counted" ((((← liveOf k "compose_system").map (·.soakUses))) == some 1)
  let tools := (jArr (reqs[0]?.getD Json.null) "tools").toList.map (jStr · "name")
  check "tool list has kernel and userspace tools" (tools.contains "propose_def" && tools.contains "word_count")

/-- 7. Lessons gain and lose score with task outcomes and retire at -3. -/
def testLessonScoring : IO Unit := do
  -- one success, then four failed tasks (script exhaustion is a kernel error)
  let k ← mkKernel (← freshDir "score") #[text "fine"]
  discard <| k.remember "parser bugs usually hide in tokenizer edge cases"
  let lid := ((← k.st).lessons.headD default).id
  let score := fun (k : Kernel) => do
    return ((← lessonById k lid).map (·.score)).getD 99
  let o ← k.runTask "debug the parser crash"
  check "first task succeeds" o.success
  check "score +1" ((← score k) == 1)
  for _ in [0:3] do discard <| k.runTask "debug the parser crash"
  check "score -2 after three failures" ((← score k) == -2)
  check "not yet retired" (!((← lessonById k lid).map (·.retired)).getD true)
  discard <| k.runTask "debug the parser crash"
  check "score -3" ((← score k) == -3)
  check "retired at -3" (((← lessonById k lid).map (·.retired)) == some true)
  mockRequests.set #[]
  discard <| k.runTask "debug the parser crash"
  check "retired lesson not scored again" ((← score k) == -3)
  -- tool error in the final tool results also fails the task
  let k2 ← mkKernel (← freshDir "score2")
    #[Json.arr #[Json.mkObj [("type", "tool_use"), ("id", "t1"), ("name", "nope"), ("input", Json.mkObj [])]],
      text "gave up"]
  discard <| k2.remember "parser hint"
  let o2 ← k2.runTask "parser task"
  check "tool error fails the task" (!o2.success)
  check "score -1" ((((← k2.st).lessons.headD default).score) == -1)

/-- 8. End-to-end `run`: the model proposes a tool and calls it within the same task. -/
def testEndToEnd : IO Unit := do
  let dir ← freshDir "e2e"
  let src := "def run (input : Lean.Json) : IO String := do\n  let t := (input.getObjValAs? String \"text\").toOption.getD \"\"\n  pure (String.ofList t.toList.reverse)\ntheorem rev_nil : String.ofList ([] : List Char).reverse = \"\" := rfl\n"
  let propose := Json.mkObj [("type", "tool_use"), ("id", "p1"), ("name", "propose_def"), ("input", Json.mkObj [
    ("name", "reverse"), ("kind", "tool"), ("source", Json.str src), ("description", "reverse a string"),
    ("tests", parseJ "[{\"input\":{\"text\":\"abc\"},\"expect\":\"cba\"}]"),
    ("input_schema", parseJ "{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}")])]
  let call := Json.mkObj [("type", "tool_use"), ("id", "c1"), ("name", "reverse"),
    ("input", parseJ "{\"text\":\"hello\"}")]
  let script := Json.arr #[Json.arr #[propose], Json.arr #[call], text "olleh",
    text "[\"Write a tool for repeated string transforms\"]"]
  let scriptPath := dir.toString ++ ".script.json"
  IO.FS.createDirAll dir
  IO.FS.writeFile scriptPath script.compress
  mockRequests.set #[]
  let code ← cliMain ["--state", dir.toString, "--provider", "mock", "--script", scriptPath, "run", "reverse hello"]
  check "run exits 0" (code == 0) s!"{code}"
  let reqs ← mockRequests.get
  check "4 requests (incl. reflection)" (reqs.size == 4) s!"{reqs.size}"
  let tools := (jArr (reqs[1]?.getD Json.null) "tools").toList.map (jStr · "name")
  check "new tool offered on the next request" (tools.contains "reverse") (toString tools)
  let toolResult := (jArr (reqs[2]?.getD Json.null) "messages").back?.map (·.compress) |>.getD ""
  check "tool result is olleh" (toolResult.contains "olleh") toolResult
  let k ← mkKernel dir
  let d ← liveOf k "reverse"
  check "reverse is live trial with 1 use" ((d.map fun d => (d.status, d.soakUses)) == some ("trial", 1))
  check "reflection lesson added" ((← k.st).lessons.any (·.text.contains "string transforms"))
  IO.FS.removeFile scriptPath

def main : IO UInt32 := do
  let tests : List (String × IO Unit) := [
    ("1 late binding", testLateBinding), ("2 gate rejection", testGateRejection),
    ("3 soak revert", testSoakRevert), ("4 soak keep", testSoakKeep),
    ("5 persistence", testPersistence), ("6 hook redefinition", testHookRedefinition),
    ("7 lesson scoring", testLessonScoring), ("8 end-to-end run", testEndToEnd)]
  let mut failed := 0
  for (name, t) in tests do
    let before := (← failures.get).size
    let t0 ← IO.monoMsNow
    try t catch e => failures.modify (·.push s!"{name}: exception {e}"); IO.eprintln s!"    exception: {e}"
    let ok := (← failures.get).size == before
    unless ok do failed := failed + 1
    IO.println s!"{if ok then "PASS" else "FAIL"} {name} ({(← IO.monoMsNow) - t0}ms)"
  IO.println s!"{tests.length - failed}/{tests.length} tests passed ({← checks.get} checks, {(← failures.get).size} failed)"
  -- clean up
  let pid ← IO.Process.getPID
  for e in ← System.FilePath.readDir "/tmp" do
    if e.fileName.startsWith s!"cmh-test-{pid}-" then
      try IO.FS.removeDirAll e.path catch _ => pure ()
  return if failed == 0 then 0 else 1
