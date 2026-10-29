import Cmh.Util
/-!
Registry and knowledge state, rebuilt purely from `journal.jsonl`.

`apply` is the only function that changes state. The live process appends an event and then
applies it, and startup replays the same events through the same function, so a replayed
state is identical to the state of the process that wrote the journal.
-/
open Lean

namespace Cmh

structure Def where
  name : String
  kind : String
  version : Nat
  source : String
  description : String
  tests : Json
  inputSchema : Json
  status : String          -- proposed | trial | kept | reverted | rejected
  soakUses : Nat := 0
  prev : Option Nat := none -- version restored if this trial reverts
  origin : String := ""    -- "seed", "rollback of vN", or ""
  error : String := ""
deriving Inhabited

def Def.toJson (d : Def) : Json :=
  Json.mkObj [("name", d.name), ("kind", d.kind), ("version", d.version), ("status", d.status),
    ("description", d.description), ("soak_uses", d.soakUses),
    ("prev", Lean.toJson d.prev),
    ("origin", d.origin), ("error", d.error), ("tests", d.tests),
    ("input_schema", d.inputSchema), ("source", d.source)]

structure Entry where
  id : Nat
  kind : String            -- lesson | verdict
  text : String
  score : Int := 0
  retired : Bool := false
deriving Inhabited

def Entry.toJson (e : Entry) : Json :=
  Json.mkObj [("id", e.id), ("kind", e.kind), ("text", e.text), ("score", e.score),
    ("retired", e.retired)]

structure TaskRec where
  id : Nat
  task : String
  success : Option Bool := none
deriving Inhabited

structure St where
  defs : Array Def := #[]
  live : List (String × Nat) := []
  knowledge : Array Entry := #[]
  tasks : Array TaskRec := #[]
deriving Inhabited

namespace St

def find? (s : St) (name : String) (v : Nat) : Option Def :=
  s.defs.find? (fun d => d.name == name && d.version == v)

def liveVersion? (s : St) (name : String) : Option Nat := s.live.lookup name

def liveDef? (s : St) (name : String) : Option Def := do
  s.find? name (← s.liveVersion? name)

def names (s : St) : List String :=
  (s.defs.toList.map (·.name)).eraseDups

def liveDefs (s : St) : List Def :=
  (s.names.filterMap s.liveDef?)

def nextVersion (s : St) (name : String) : Nat :=
  (s.defs.foldl (fun m d => if d.name == name then max m d.version else m) 0) + 1

def updDef (s : St) (name : String) (v : Nat) (f : Def → Def) : St :=
  { s with defs := s.defs.map (fun d => if d.name == name && d.version == v then f d else d) }

def setLive (s : St) (name : String) (v : Option Nat) : St :=
  let rest := s.live.filter (·.1 != name)
  { s with live := match v with | some v => rest ++ [(name, v)] | none => rest }

def updEntry (s : St) (id : Nat) (f : Entry → Entry) : St :=
  { s with knowledge := s.knowledge.map (fun e => if e.id == id then f e else e) }

def lessons (s : St) : List Entry := s.knowledge.toList.filter (·.kind == "lesson")
def activeLessons (s : St) : List Entry := s.lessons.filter (!·.retired)
def verdicts (s : St) : List Entry := s.knowledge.toList.filter (·.kind == "verdict")

/-- Lessons, verdicts, registry and soak counters: what persistence must reproduce. -/
def snapshot (s : St) : Json :=
  Json.mkObj [("defs", Lean.toJson (s.defs.map Def.toJson)),
    ("live", Lean.toJson (s.live.map fun (n, v) => Json.mkObj [("name", n), ("version", v)])),
    ("knowledge", Lean.toJson (s.knowledge.map Entry.toJson)),
    ("tasks", Lean.toJson (s.tasks.map fun t => Json.mkObj [("id", t.id), ("task", t.task),
      ("success", Lean.toJson t.success)]))]

end St

/-- Apply one journal event. Unknown event types are ignored. -/
def apply (s : St) (ev : Json) : St :=
  let name := jStr ev "name"
  let ver := (jNat? ev "version").getD 0
  let id := (jNat? ev "id").getD 0  -- lesson and task events
  match jStr ev "type" with
  | "def_proposed" =>
    let d : Def := {
        name, version := ver, kind := jStr ev "kind", source := jStr ev "source",
        description := jStr ev "description", tests := ev.getObjValD "tests",
        inputSchema := ev.getObjValD "input_schema", status := "proposed", origin := jStr ev "origin" }
    { s with defs := s.defs.push d }
  | "def_rejected" => s.updDef name ver (fun d => { d with status := "rejected", error := jStr ev "error" })
  | "def_trial" =>
    (s.updDef name ver (fun d => { d with status := "trial", soakUses := 0, prev := jNat? ev "prev" })).setLive
      name (some ver)
  | "def_use" =>
    if jBool ev "ok" then
      s.updDef name ver (fun d => if d.status == "trial" then { d with soakUses := d.soakUses + 1 } else d)
    else s
  | "def_kept" => (s.updDef name ver (fun d => { d with status := "kept" })).setLive name (some ver)
  | "def_reverted" =>
    (s.updDef name ver (fun d => { d with status := "reverted", error := jStr ev "error" })).setLive name
      (jNat? ev "restored")
  | "lesson_added" =>
    let e : Entry := { id, kind := jStr ev "kind" "lesson", text := jStr ev "text" }
    { s with knowledge := s.knowledge.push e }
  | "lesson_scored" => s.updEntry id (fun d => { d with score := jInt ev "score" })
  | "lesson_retired" => s.updEntry id (fun d => { d with retired := true })
  | "task_started" =>
    let t : TaskRec := { id, task := jStr ev "task" }
    { s with tasks := s.tasks.push t }
  | "task_finished" =>
    { s with tasks := s.tasks.map fun t =>
        if t.id == id then { t with success := some (jBool ev "success") } else t }
  | _ => s

/-- Read every event of the journal (a torn final line from a crash is skipped). -/
def readJournal (path : System.FilePath) : IO (Array Json) := do
  unless ← path.pathExists do return #[]
  let lines ← IO.FS.lines path
  let mut out := #[]
  for l in lines do
    if strTrim l == "" then continue
    match Json.parse l with
    | .ok j => out := out.push j
    | .error e => IO.eprintln s!"[journal] skipping unreadable line: {e}"
  return out

def appendJournal (path : System.FilePath) (ev : Json) : IO Unit := do
  let h ← IO.FS.Handle.mk path .append
  h.putStrLn ev.compress
  h.flush

end Cmh
