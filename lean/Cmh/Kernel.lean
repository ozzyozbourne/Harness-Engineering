import Cmh.State
import Cmh.Elab
import Cmh.Provider
import Cmh.Embedded
import Std.Data.HashMap
/-!
The kernel: gate, hooks, kernel tools, agent loop, reflection and lesson scoring.
Userspace is always resolved through the registry by name at call time.
-/
open Lean

namespace Cmh

structure Config where
  stateDir : System.FilePath := ".cmh"
  workDir : System.FilePath := "."
  soak : Nat := 3
  reflect : Bool := true
  callTimeoutMs : Nat := 30000
  maxTurns : Nat := 60
  /-- Print assistant text to stdout as it arrives (repl). `run` prints only the final answer. -/
  echo : Bool := false
  trace : Bool := true

structure Kernel where
  cfg : Config
  stRef : IO.Ref St
  /-- Compiled closures by (name, version); rebuildable, since every entry comes from journaled source. -/
  cache : IO.Ref (Std.HashMap (String × Nat) Compiled)
  provider : Provider
  basePrompt : String
  protectedDirs : Array System.FilePath

def kernelToolSchemas : Array Json :=
  let obj (props : List (String × Json)) (req : List String) : Json :=
    Json.mkObj [("type", "object"), ("properties", Json.mkObj props), ("required", toJson req)]
  let str (d : String) : Json := Json.mkObj [("type", "string"), ("description", d)]
  let int (d : String) : Json := Json.mkObj [("type", "integer"), ("description", d)]
  let tool (n d : String) (schema : Json) : Json :=
    Json.mkObj [("name", n), ("description", d), ("input_schema", schema)]
  #[tool "bash" "Run a shell command in the working directory; returns combined output and exit code (20 KB max)."
      (obj [("command", str "shell command"), ("timeout_s", int "timeout in seconds, default 60")] ["command"]),
    tool "read_file" "Read a file (50 KB max)." (obj [("path", str "file path")] ["path"]),
    tool "write_file" "Write a file (not inside the state dir or kernel source)."
      (obj [("path", str "file path"), ("content", str "full file content")] ["path", "content"]),
    tool "list_defs" "List userspace definitions: name, kind, live version, status, description." (obj [] []),
    tool "show_def" "Show a definition version's full record including source and tests."
      (obj [("name", str "definition name"), ("version", int "version, default live")] ["name"]),
    -- the contract wording repeats the signatures in `Cmh.contract?` (Elab.lean)
    tool "propose_def" ("Propose a new version of a userspace definition (Lean 4). It is elaborated in a scratch " ++
      "environment (imports Init and Lean.Data.Json only; no import lines), every theorem/example must check, " ++
      "sorry/axiom/unsafe/implemented_by/extern/#eval are rejected, and all tests must pass. Then it goes live as " ++
      "a trial and is kept after clean uses or reverted on the first error. Contracts: tool -> " ++
      "`def run (input : Lean.Json) : IO String` (read fields with e.g. `input.getObjValAs? String \"text\"`); " ++
      "hook compose_system -> `def composeSystem (notes lessons : List String) : String`; hook select_lessons -> " ++
      "`def selectLessons (task : String) (lessons : List (Nat × String × Int)) : List Nat` (id, text, score); " ++
      "note -> plain text. Tests: [{\"input\": {...}, \"expect\": \"substring of output\"}].")
      (obj [("name", str "definition name, [A-Za-z][A-Za-z0-9_]*"),
            ("kind", Json.mkObj [("type", "string"), ("enum", toJson ["tool", "note", "hook"])]),
            ("source", str "Lean 4 source (or note text)"), ("description", str "one-line description"),
            ("tests", Json.mkObj [("type", "array"), ("items", Json.mkObj [("type", "object")])]),
            ("input_schema", Json.mkObj [("type", "object"), ("description", "JSON Schema of the tool input (tools only)")])]
        ["name", "kind", "source", "description"]),
    tool "rollback_def" "Reinstall an older version as a new trial version."
      (obj [("name", str "definition name"), ("version", int "version to reinstall")] ["name", "version"]),
    tool "remember" "Record a durable, reusable lesson." (obj [("text", str "the lesson")] ["text"]),
    tool "recall" "Search lessons and verdicts sharing words with the query." (obj [("query", str "query")] ["query"])]

def kernelToolNames : List String := kernelToolSchemas.toList.map (jStr · "name")

/-- The only names a hook can have; nothing else may use them. -/
def hookNames : List String := ["compose_system", "select_lessons"]

/-- Package directory (`lean/`): the executable lives in `<pkg>/.lake/build/bin/`. -/
def packageDir : IO (Option System.FilePath) := do
  let app ← IO.appPath
  let pkg := app.parent >>= (·.parent) >>= (·.parent) >>= (·.parent)
  match pkg with
  | some p => if ← (p / "lakefile.lean").pathExists then return some p else return none
  | none => return none

/-- The disk copy of a package file if there is one, else its build-time embedded copy. -/
def readPackageFile (pkg : Option System.FilePath) (rel : System.FilePath) (embedded : String) :
    IO String := do
  if let some p := pkg then
    if ← (p / rel).pathExists then return ← IO.FS.readFile (p / rel)
  return embedded

namespace Kernel

def st (k : Kernel) : IO St := k.stRef.get

def trace (k : Kernel) (s : String) : IO Unit := do
  if k.cfg.trace then IO.eprintln s

def journal (k : Kernel) : System.FilePath := k.cfg.stateDir / "journal.jsonl"

/-- Append an event to the journal and apply it to the in-memory state. -/
def emit (k : Kernel) (type : String) (fields : List (String × Json)) : IO Unit := do
  let ev := Json.mkObj ([("type", Json.str type), ("ts", Json.str (← nowIso))] ++ fields)
  appendJournal k.journal ev
  k.stRef.modify (apply · ev)

def addEntry (k : Kernel) (kind text : String) : IO Nat := do
  let id := (← k.st).knowledge.size + 1
  k.emit "lesson_added" [("id", id), ("kind", kind), ("text", text)]
  return id

/-! ## Compiled-closure cache (rebuildable: every entry comes from a journal source) -/

def cachePut (k : Kernel) (name : String) (version : Nat) (c : Compiled) : IO Unit :=
  k.cache.modify (·.insert (name, version) c)

/-- The closure for a definition, elaborating it on first use (e.g. after a journal replay). -/
def getCompiled (k : Kernel) (d : Def) : IO (Except String Compiled) := do
  if let some c := (← k.cache.get)[(d.name, d.version)]? then return .ok c
  let r ← elaborate d.name d.kind d.version d.source
  if let .ok c := r then k.cachePut d.name d.version c
  return r

/-! ## Soak bookkeeping -/

def verdictLabel (d : Def) : String := s!"def {d.name} v{d.version} (\"{strTake d.description 120}\")"

/-- Record one use of a definition. Only trial versions are journaled and can change status. -/
def recordUse (k : Kernel) (d : Def) (err : Option String) (input : Json) : IO Unit := do
  let some cur := (← k.st).find? d.name d.version | return
  unless cur.status == "trial" do return
  k.emit "def_use" [("name", d.name), ("version", d.version), ("ok", err.isNone),
    ("error", toJson (err.map (strTake · 500)))]
  match err with
  | some e =>
    let s ← k.st
    let restored := cur.prev.filter (fun v => (s.find? d.name v).isSome)
    k.emit "def_reverted" [("name", d.name), ("version", d.version), ("error", strTake e 500),
      ("restored", toJson restored)]
    let inp := if input.isNull then "" else s!" on input {strTake input.compress 200}"
    discard <| k.addEntry "verdict" s!"{verdictLabel cur} reverted: {strTake e 300}{inp}"
    let to := match restored with | some v => s!"v{v}" | none => "removed"
    k.trace s!"[gate] {d.name} v{d.version} reverted -> {to}: {strTake (e.replace "\n" " ") 160}"
  | none =>
    -- re-read on purpose: the `def_use` event above just bumped `soakUses`
    let some now := (← k.st).find? d.name d.version | return
    if now.soakUses ≥ k.cfg.soak then
      k.emit "def_kept" [("name", d.name), ("version", d.version)]
      discard <| k.addEntry "verdict" s!"{verdictLabel now} kept after {now.soakUses} clean uses"
      k.trace s!"[gate] {d.name} v{d.version} kept"

/-! ## Hooks -/

/-- A markdown section with one bullet per item, or nothing if there are no items. -/
def mdSection (title : String) (items : List String) : List String :=
  if items.isEmpty then [] else ["## " ++ title ++ "\n" ++ String.intercalate "\n" (items.map ("- " ++ ·))]

/-! `builtinCompose` and `builtinSelect` deliberately duplicate the seeds in `userspace/`: seeds are
elaborated in their own isolated environment, so the kernel cannot call them, and these copies are
the fallback when no working hook version is live. -/

def builtinCompose (notes lessons : List String) : String :=
  String.intercalate "\n\n" (mdSection "Notes" notes ++ mdSection "Lessons" lessons)

def builtinSelect (task : String) (ls : List (Nat × String × Int)) : List Nat :=
  let tw := longWords task
  let sorted := ls.mergeSort (fun a b => decide (a.2.2 ≥ b.2.2))
  let m := sorted.filter (fun l => (longWords l.2.1).any (tw.contains ·))
  (if m.isEmpty then sorted.take 3 else m.take 8).map (·.1)

/-! Hook inputs as JSON: the encoding is what a failed hook call journals in its verdict, the
decoding is how `validate` feeds a hook's test inputs. Keep each pair in sync. -/

def composeInput (notes lessons : List String) : Json :=
  Json.mkObj [("notes", toJson notes), ("lessons", toJson lessons)]

def composeArgs (input : Json) : List String × List String :=
  let strings (key : String) := (jArr input key).toList.map fun j => match j with
    | .str s => s | _ => j.compress
  (strings "notes", strings "lessons")

def selectInput (task : String) (ls : List (Nat × String × Int)) : Json :=
  Json.mkObj [("task", task), ("lessons", toJson (ls.map fun (i, t, s) =>
    Json.mkObj [("id", i), ("text", t), ("score", s)]))]

def selectArgs (input : Json) : String × List (Nat × String × Int) :=
  let ls := (jArr input "lessons").toList.map fun l => ((jNat? l "id").getD 0, jStr l "text", jInt l "score")
  (jStr input "task", ls)

/-- Call the live version `d` and record the use. `pick` extracts the action from the compiled
closure, or returns `none` if `d` compiled to another kind (then the call fails with `wrongKind`).
`input` is only built if the call fails, for the verdict; `onError` runs before the use is recorded. -/
def callLive (k : Kernel) (d : Def) (input : Unit → Json) (wrongKind : String)
    (pick : Compiled → Option (IO α)) (onError : String → IO Unit := fun _ => pure ()) :
    IO (Except String α) := do
  let r ← match ← k.getCompiled d with
    | .ok c => match pick c with
      | some act => runTimed k.cfg.callTimeoutMs act
      | none => pure (.error wrongKind)
    | .error e => pure (.error e)
  match r with
  | .ok _ => k.recordUse d none Json.null
  | .error e => onError e; k.recordUse d (some e) (input ())
  return r

/-- Call the live version of hook `name`. If it fails and was a trial, the failure has just
reverted it, so the restored version (if any) is tried next; a failing kept version falls back
to `builtin`, as does having no live version at all. -/
partial def callHook (k : Kernel) (name : String) (input : Unit → Json) (builtin : α)
    (pick : Compiled → Option (IO α)) : IO α := do
  let some d := (← k.st).liveDef? name | return builtin
  let r ← k.callLive d input s!"bad return type: not a {name} hook" pick
    (onError := fun e => k.trace s!"[hook] {name} v{d.version} error: {strTake e 200}")
  match r with
  | .ok v => return v
  | .error _ => if d.status == "trial" then k.callHook name input builtin pick else return builtin

def composeSystem (k : Kernel) (notes lessons : List String) : IO String :=
  k.callHook "compose_system" (fun _ => composeInput notes lessons) (builtinCompose notes lessons)
    fun | .compose f => some (IO.lazyPure fun _ => f notes lessons) | _ => none

/-- Throws (so the call counts as failed) if the hook returns an id that was not offered. -/
def selectLessons (k : Kernel) (task : String) (ls : List (Nat × String × Int)) : IO (List Nat) :=
  k.callHook "select_lessons" (fun _ => selectInput task ls) (builtinSelect task ls)
    fun | .select f => some do
          let ids ← IO.lazyPure fun _ => f task ls
          unless ids.all (fun i => ls.any (·.1 == i)) do
            throw (IO.userError s!"bad return: unknown lesson id in {ids}")
          return ids.eraseDups
        | _ => none

/-! ## The gate -/

def validName (n : String) : Bool :=
  !n.isEmpty && n.length ≤ 64 && (n.toList.headD '0').isAlpha &&
    n.toList.all (fun c => c.isAlphanum || c == '_')

/-- Accept JSON values that the model may have sent as JSON-encoded strings. -/
def jsonish (j : Json) : Json :=
  match j with
  | .str s => match Json.parse s with | .ok v => v | .error _ => j
  | _ => j

/-- Run a compiled definition on one test input. The output is the text a test's `expect`
substring is searched in; `select_lessons` ids are rendered as a JSON array. -/
def runOnTestInput (k : Kernel) (c : Compiled) (input : Json) : IO (Except String String) :=
  match c with
  | .tool f => runTimed k.cfg.callTimeoutMs (f input)
  | .compose f =>
    let (notes, lessons) := composeArgs input
    runTimed k.cfg.callTimeoutMs (IO.lazyPure fun _ => f notes lessons)
  | .select f =>
    let (task, ls) := selectArgs input
    (·.map fun ids => (toJson ids).compress) <$>
      runTimed k.cfg.callTimeoutMs (IO.lazyPure fun _ => f task ls)
  | .note => pure (.ok "")

/-- Validate a proposal in a scratch environment: elaborate it, then run its tests. -/
def validate (k : Kernel) (name kind : String) (version : Nat) (source : String) (tests : Json) :
    IO (Except String Compiled) := do
  let tests := tests.getArr?.toOption.getD #[]
  if kind != "note" && tests.isEmpty then
    return .error "tools and hooks need at least one test: [{\"input\": {...}, \"expect\": \"...\"}]"
  match ← elaborate name kind version source with
  | .error e => return .error e
  | .ok c =>
    for t in tests, i in [1:tests.size + 1] do
      let input := t.getObjValD "input"
      let expect := match t.getObjValD "expect" with | .str s => s | j => j.compress
      match ← k.runOnTestInput c input with
      | .error e => return .error s!"test {i} raised: {e} (input {strTake input.compress 200})"
      | .ok o =>
        unless o.contains expect do
          return .error s!"test {i} failed: expected substring {expect.quote} in output {(strTake o 500).quote} \
            (input {strTake input.compress 200})"
    return .ok c

/-- Run the gate on a proposal and journal the outcome. Returns the verdict text and whether it
went live. -/
def propose (k : Kernel) (name kind source description : String) (tests inputSchema : Json)
    (origin : String := "") : IO (String × Bool) := do
  let s ← k.st
  -- rejected before a version is allocated: nothing is journaled
  let reject (msg : String) : IO (String × Bool) := pure (s!"rejected: {msg}", false)
  unless validName name do return ← reject "name must match [A-Za-z][A-Za-z0-9_]* (max 64)"
  unless ["tool", "note", "hook"].contains kind do return ← reject "kind must be tool, note or hook"
  if kind == "hook" && !hookNames.contains name then
    return ← reject "hook name must be compose_system or select_lessons"
  if kind != "hook" && hookNames.contains name then
    return ← reject s!"{name} is a hook name; use kind hook"
  if kernelToolNames.contains name then return ← reject s!"{name} is a kernel tool and cannot be redefined"
  if let some old := s.defs.find? (·.name == name) then
    if old.kind != kind then return ← reject s!"{name} already exists with kind {old.kind}"
  let schema := if kind == "tool" then
      (match inputSchema with | .obj _ => inputSchema | _ => Json.mkObj [("type", "object"), ("properties", Json.mkObj [])])
    else Json.null
  let version := s.nextVersion name
  k.emit "def_proposed" [("name", name), ("kind", kind), ("version", version), ("source", source),
    ("description", description), ("tests", tests), ("input_schema", schema), ("origin", origin)]
  let t0 ← IO.monoMsNow
  match ← k.validate name kind version source tests with
  | .error e =>
    let shown := strTake e 4000
    k.emit "def_rejected" [("name", name), ("version", version), ("error", shown)]
    k.trace s!"[gate] {name} v{version} rejected ({(← IO.monoMsNow) - t0}ms): {strTake (e.replace "\n" " ") 160}"
    return (s!"rejected v{version}; live version unchanged.\n{shown}", false)
  | .ok c =>
    k.cachePut name version c
    -- `s` is still current for `live`: proposing and validating never change it.
    -- A trial replacing another trial restores what that trial would have restored.
    let prev := match s.liveDef? name with
      | some d => if d.status == "kept" then some d.version else d.prev
      | none => none
    k.emit "def_trial" [("name", name), ("version", version), ("prev", toJson prev)]
    k.trace s!"[gate] {name} v{version} trial (validated in {(← IO.monoMsNow) - t0}ms)"
    return (s!"trial v{version}: validated and live now; kept after {k.cfg.soak} clean uses, \
      reverted on the first error", true)

def rollback (k : Kernel) (name : String) (version : Nat) : IO (String × Bool) := do
  let some d := (← k.st).find? name version | return (s!"no such definition {name} v{version}", false)
  k.propose name d.kind d.source d.description d.tests d.inputSchema (origin := s!"rollback of v{version}")

/-! ## Kernel tools -/

def resolvePath (p : String) : IO System.FilePath := do
  let fp : System.FilePath := p
  let cwd ← IO.currentDir
  let abs := if fp.isAbsolute then fp else cwd / fp
  try
    IO.FS.realPath abs
  catch _ =>
    match abs.parent, abs.fileName with
    | some par, some f =>
      try
        let rp ← IO.FS.realPath par
        return rp / f
      catch _ => return abs.normalize
    | _, _ => return abs.normalize

def isUnder (p dir : System.FilePath) : Bool :=
  let pc := p.normalize.components.filter (· != "")
  let dc := dir.normalize.components.filter (· != "")
  pc.take dc.length == dc

def bash (k : Kernel) (cmd : String) (timeoutS : Nat) : IO (String × Bool) := do
  let child ← IO.Process.spawn {
    cmd := "timeout", args := #["-k", "5", toString timeoutS, "sh", "-c", "exec 2>&1\n" ++ cmd]
    cwd := k.cfg.workDir, stdin := .null, stdout := .piped, stderr := .null }
  let out ← child.stdout.readToEnd
  let code ← child.wait
  let note := if code == 124 || code == 137 then s!" (timed out after {timeoutS}s)" else ""
  return (truncate out 20000 ++ s!"\n[exit code {code}{note}]", code == 124 || code == 137)

def listDefs (k : Kernel) : IO String := do
  let s ← k.st
  let lines := s.names.map fun n =>
    match s.liveDef? n with
    | some d => s!"{d.name}  {d.kind}  v{d.version}  {d.status}{if d.status == "trial" then s!" ({d.soakUses}/{k.cfg.soak})" else ""}  {d.description}"
    | none =>
      let kind := (s.defs.find? (·.name == n)).map (·.kind) |>.getD "?"
      s!"{n}  {kind}  -  (no live version)"
  return if lines.isEmpty then "(no definitions)" else String.intercalate "\n" lines

def recallText (k : Kernel) (query : String) : IO String := do
  let qw := (words query).filter (·.length ≥ 3)
  let s ← k.st
  let hits := s.knowledge.toList.filter fun e =>
    !(e.kind == "lesson" && e.retired) && (words e.text).any (qw.contains ·)
  let hits := hits.mergeSort (fun a b => decide (a.score ≥ b.score))
  if hits.isEmpty then return "no matching lessons or verdicts"
  return String.intercalate "\n" (hits.take 20 |>.map fun e => s!"[{e.id}] ({e.kind}, score {e.score}) {e.text}")

def remember (k : Kernel) (text : String) : IO String := do
  let t := strTrim text
  if t.isEmpty then return "empty lesson ignored"
  if (← k.st).lessons.any (fun e => strTrim e.text.toLower == t.toLower) then
    return "already known"
  let id ← k.addEntry "lesson" t
  return s!"lesson {id} recorded"

/-- The full record of a version (default: the live one) as pretty JSON. Returns (text, isError). -/
def showDef (k : Kernel) (name : String) (version? : Option Nat) : IO (String × Bool) := do
  let s ← k.st
  match (version?.orElse fun _ => s.liveVersion? name) >>= (s.find? name ·) with
  | some d => return (d.toJson.pretty, false)
  | none => return (s!"no such definition {name}", true)

/-- Run a kernel tool. Returns (output, isError). -/
def kernelTool (k : Kernel) (name : String) (input : Json) : IO (String × Bool) := do
  match name with
  | "bash" => k.bash (jStr input "command") ((jNat? input "timeout_s").getD 60)
  | "read_file" =>
    let p ← resolvePath (jStr input "path")
    return (truncate (← IO.FS.readFile p) 50000, false)
  | "write_file" =>
    let p ← resolvePath (jStr input "path")
    if k.protectedDirs.any (isUnder p ·) then
      return (s!"refused: {p} is inside the state dir or the kernel source", true)
    if let some par := p.parent then IO.FS.createDirAll par
    let content := jStr input "content"
    IO.FS.writeFile p content
    return (s!"wrote {content.utf8ByteSize} bytes to {p}", false)
  | "list_defs" => return (← k.listDefs, false)
  | "show_def" => k.showDef (jStr input "name") (jNat? input "version")
  | "propose_def" =>
    let (msg, ok) ← k.propose (jStr input "name") (jStr input "kind") (jStr input "source")
      (jStr input "description") (jsonish (input.getObjValD "tests")) (jsonish (input.getObjValD "input_schema"))
    return (msg, !ok)
  | "rollback_def" =>
    let (msg, ok) ← k.rollback (jStr input "name") ((jNat? input "version").getD 0)
    return (msg, !ok)
  | "remember" => return (← k.remember (jStr input "text"), false)
  | "recall" => return (← k.recallText (jStr input "query"), false)
  | _ => return (s!"unknown tool {name}", true)

def userToolSchemas (k : Kernel) : IO (Array Json) := do
  return ((← k.st).liveDefs.filter (·.kind == "tool")).toArray.map fun d =>
    Json.mkObj [("name", d.name), ("description", d.description), ("input_schema", d.inputSchema)]

/-- The most telling string argument of a tool call, for the trace line. -/
def shortArg (input : Json) : String :=
  let s := (jStr? input "name" <|> jStr? input "command" <|> jStr? input "path").getD input.compress
  strTake (s.replace "\n" " ") 60

/-- Dispatch one tool call (kernel tools first, then the live userspace registry by name). -/
def callTool (k : Kernel) (name : String) (input : Json) : IO (String × Bool) := do
  let (out, isErr) ← if kernelToolNames.contains name then
      try k.kernelTool name input catch e => pure (s!"error: {e}", true)
    else match (← k.st).liveDef? name with
      | some d =>
        if d.kind != "tool" then pure (s!"{name} is not a tool", true) else do
        match ← k.callLive d (fun _ => input) "bad return type" fun | .tool f => some (f input) | _ => none with
        | .ok s => pure (s, false)
        | .error e => pure (s!"error: {e}", true)
      | none => pure (s!"unknown tool {name}", true)
  let summary := if name == "propose_def" || name == "rollback_def" then
      (out.splitOn "\n").headD "" |>.splitOn ":" |>.headD "" else if isErr then s!"error: {strTake (out.replace "\n" " ") 100}" else "ok"
  k.trace s!"[tool] {name} {shortArg input} -> {summary}"
  return (out, isErr)

/-! ## Agent loop -/

structure Outcome where
  success : Bool
  answer : String
  error : Option String := none

def textOf (content : Array Json) : String :=
  String.intercalate "\n" (content.toList.filterMap fun b =>
    if jStr b "type" == "text" then some (jStr b "text") else none)

def buildSystem (k : Kernel) (selected : List String) : IO (Array String) := do
  let s ← k.st
  let notes := (s.liveDefs.filter (·.kind == "note")).map (·.source)
  let supp ← k.composeSystem notes selected
  let vs := s.verdicts
  let vs := vs.drop (vs.length - 20)
  let verdicts := String.intercalate "\n\n" (mdSection "Verdicts on past userspace changes" (vs.map (·.text)))
  let supp := String.intercalate "\n\n" ([supp, verdicts].filter (· != ""))
  return #[k.basePrompt] ++ (if supp.isEmpty then #[] else #[supp])

def compactTranscript (task : String) (msgs : Array Json) (o : Outcome) : String := Id.run do
  let mut out := s!"TASK: {strTake task 1000}\n"
  for m in msgs.toList.drop 1 do
    for b in jArr m "content" do
      match jStr b "type" with
      | "text" => out := out ++ s!"ASSISTANT: {strTake (jStr b "text") 400}\n"
      | "tool_use" => out := out ++ s!"TOOL CALL {jStr b "name"}: {strTake (b.getObjValD "input").compress 300}\n"
      | "tool_result" =>
        out := out ++ s!"TOOL RESULT{if jBool b "is_error" then " (error)" else ""}: {strTake (jStr b "content") 300}\n"
      | _ => pure ()
  out := out ++ s!"OUTCOME: {if o.success then "success" else "failure"}{match o.error with | some e => s!" ({strTake e 200})" | none => ""}\n"
  return out

/-- Up to 3 non-empty lessons from the reflection reply: the JSON string array spanning from the
first `[` to the last `]`, ignoring any prose around it. -/
def parseLessons (text : String) : List String :=
  let fromOpen := text.toList.dropWhile (· != '[')
  let body := String.ofList (fromOpen.reverse.dropWhile (· != ']')).reverse
  match Json.parse body with
  | .ok (.arr a) => (a.toList.filterMap fun j => match j with | .str s => some (strTrim s) | _ => none).filter (· != "") |>.take 3
  | _ => []

def reflect (k : Kernel) (task : String) (msgs : Array Json) (o : Outcome) : IO Unit := do
  let prompt := compactTranscript task msgs o ++
    "\nReflect on this finished task. Reply with only a JSON array of 0 to 3 short, reusable lessons " ++
    "(strings) that would help with future tasks. Do not record task-specific trivia. Reply [] if none."
  try
    let resp ← k.provider.complete { system := #[k.basePrompt], messages := #[Json.mkObj [("role", "user"), ("content", prompt)]] }
    let mut added := 0
    for l in parseLessons (textOf resp.content) do
      if (← k.remember l).startsWith "lesson" then added := added + 1
    k.trace s!"[reflect] {added} lesson(s) added"
  catch e => k.trace s!"[reflect] failed: {e}"

/-- What the conversation loop leaves behind for scoring and reflection. -/
structure Conversation where
  msgs : Array Json
  /-- One flag per tool call, in order: whether it returned an error. -/
  toolErrors : Array Bool := #[]
  /-- Why the model did not finish with `end_turn`; `none` if it did. -/
  kernelErr : Option String
  /-- Text of the final assistant message. -/
  answer : String := ""

def toolResultBlock (id out : String) (isErr : Bool) : Json :=
  Json.mkObj ([("type", Json.str "tool_result"), ("tool_use_id", Json.str id),
    ("content", Json.str (if out.isEmpty then "(empty)" else out))] ++
    (if isErr then [("is_error", Json.bool true)] else []))

/-- Talk to the model until it stops asking for tools, the request fails, or the turn limit. -/
def converse (k : Kernel) (task : String) (lessons : List String) : IO Conversation := do
  let mut c : Conversation := { msgs := #[Json.mkObj [("role", "user"), ("content", task)]],
                                kernelErr := some s!"turn limit ({k.cfg.maxTurns}) reached" }
  for _ in [0:k.cfg.maxTurns] do
    let req : Request := { system := ← k.buildSystem lessons,
                           tools := kernelToolSchemas ++ (← k.userToolSchemas), messages := c.msgs }
    let resp ← try pure (Except.ok (← k.provider.complete req)) catch e => pure (.error (toString e))
    match resp with
    | .error e =>
      k.trace s!"[kernel] model request failed: {strTake e 300}"
      return { c with kernelErr := some e }
    | .ok resp =>
      c := { c with msgs := c.msgs.push (Json.mkObj [("role", "assistant"), ("content", toJson resp.content)]) }
      let txt := textOf resp.content
      if k.cfg.echo && txt != "" then IO.println txt
      let uses := resp.content.filter (jStr · "type" == "tool_use")
      unless resp.stopReason == "tool_use" && !uses.isEmpty do
        let kernelErr := if resp.stopReason == "end_turn" then none else some s!"stopped with {resp.stopReason}"
        return { c with answer := txt, kernelErr }
      let mut results := #[]
      for u in uses do
        let (out, isErr) ← k.callTool (jStr u "name") (u.getObjValD "input")
        c := { c with toolErrors := c.toolErrors.push isErr }
        results := results.push (toolResultBlock (jStr u "id") out isErr)
      c := { c with msgs := c.msgs.push (Json.mkObj [("role", "user"), ("content", toJson results)]) }
  return c

/-- Score the lessons that were in the prompt by the task outcome; retire them at -3. -/
def scoreLessons (k : Kernel) (selected : List Entry) (success : Bool) : IO Unit := do
  for e in selected do
    let some cur := (← k.st).knowledge.find? (·.id == e.id) | continue
    let delta : Int := if success then 1 else -1
    k.emit "lesson_scored" [("id", e.id), ("delta", delta), ("score", cur.score + delta)]
    if cur.score + delta ≤ -3 && !cur.retired then
      k.emit "lesson_retired" [("id", e.id)]
      k.trace s!"[lesson] {e.id} retired"

/-- Notes have no calls of their own: each task outcome counts as one use of every trial note. -/
def soakNotes (k : Kernel) (taskId : Nat) (success : Bool) : IO Unit := do
  for d in (← k.st).liveDefs.filter (fun d => d.kind == "note" && d.status == "trial") do
    k.recordUse d (if success then none else some s!"task {taskId} failed") Json.null

def runTask (k : Kernel) (task : String) : IO Outcome := do
  let tid := (← k.st).tasks.size + 1
  k.emit "task_started" [("id", tid), ("task", task)]
  let active := (← k.st).activeLessons
  let ids ← k.selectLessons task (active.map fun e => (e.id, e.text, e.score))
  let selected := ids.filterMap fun i => active.find? (·.id == i)
  let c ← k.converse task (selected.map (·.text))
  -- an error in the last two tool results means the task is not trusted as a success
  let lastErr := (c.toolErrors.toList.drop (c.toolErrors.size - 2)).any id
  let success := c.kernelErr.isNone && !lastErr
  let outcome : Outcome := { success, answer := c.answer, error := c.kernelErr }
  k.emit "task_finished" [("id", tid), ("success", success), ("summary", strTake c.answer 300),
    ("error", toJson (c.kernelErr.map (strTake · 300)))]
  k.scoreLessons selected success
  k.soakNotes tid success
  k.trace s!"[task] {tid} {if success then "succeeded" else "failed"}{match c.kernelErr with | some e => s!": {strTake e 200}" | none => ""}"
  if k.cfg.reflect then k.reflect task c.msgs outcome
  return outcome

def refinePrompt (k : Kernel) : IO String := do
  let s ← k.st
  let recent := (s.tasks.toList.drop (s.tasks.size - 10)).map fun t =>
    s!"- task {t.id}: {match t.success with | some true => "success" | some false => "FAILED" | none => "unfinished"}: {strTake (t.task.replace "\n" " ") 120}"
  return "Refine yourself. Review the recent task outcomes below, the verdicts in your system prompt, and " ++
    "your current userspace (use list_defs and show_def). Then improve userspace through propose_def or " ++
    "knowledge through remember. Make at most 3 changes and prefer small ones. Finish with a short summary " ++
    "of what you changed and why.\n\nRecent task outcomes:\n" ++
    (if recent.isEmpty then "(none yet)" else String.intercalate "\n" recent)

/-- Install the seed userspace as kept version 1 (only when the journal is empty). -/
def installSeeds (k : Kernel) (pkg : Option System.FilePath) : IO Unit := do
  let .ok (.arr seeds) := Json.parse Embedded.seedManifest | throw (IO.userError "bad seed manifest")
  for sd in seeds do
    let file := jStr sd "file"
    let src ← readPackageFile pkg ("userspace" / file) ((Embedded.seedSource file).getD "")
    k.emit "def_proposed" [("name", jStr sd "name"), ("kind", jStr sd "kind"), ("version", (1 : Nat)),
      ("source", src), ("description", jStr sd "description"), ("tests", sd.getObjValD "tests"),
      ("input_schema", sd.getObjValD "input_schema"), ("origin", "seed")]
    k.emit "def_kept" [("name", jStr sd "name"), ("version", (1 : Nat))]

/-- Open (or create) a state dir: replay the journal, seeding userspace if it is empty. -/
def «open» (cfg : Config) (provider : Provider) : IO Kernel := do
  IO.FS.createDirAll cfg.stateDir
  let stateDir ← IO.FS.realPath cfg.stateDir
  let cfg := { cfg with stateDir }
  let events ← readJournal (stateDir / "journal.jsonl")
  let st := events.foldl apply {}
  let pkg ← packageDir
  let mut prot := #[stateDir]
  if let some p := pkg then
    prot := prot ++ #[p / "Cmh", p / "Main.lean", p / "lakefile.lean"]
  let basePrompt ← readPackageFile pkg (".." / "base-prompt.md") Embedded.basePrompt
  let k : Kernel := { cfg, stRef := ← IO.mkRef st, cache := ← IO.mkRef {}, provider,
                      basePrompt, protectedDirs := prot }
  if events.isEmpty then k.installSeeds pkg
  return k

def knowledgeText (k : Kernel) : IO String := do
  let s ← k.st
  let ls := s.lessons.mergeSort (fun a b => decide (a.score ≥ b.score))
  let lessonLines := ls.map fun e => s!"[{e.id}] score {e.score}{if e.retired then " (retired)" else ""}  {e.text}"
  let vLines := s.verdicts.map fun e => s!"[{e.id}] {e.text}"
  return "Lessons:\n" ++ (if ls.isEmpty then "  (none)" else String.intercalate "\n" (lessonLines.map ("  " ++ ·))) ++
    "\nVerdicts:\n" ++ (if vLines.isEmpty then "  (none)" else String.intercalate "\n" (vLines.map ("  " ++ ·)))

end Kernel
end Cmh
