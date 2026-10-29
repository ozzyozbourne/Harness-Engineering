import Lean
import Cmh.Util
/-!
Runtime elaboration of userspace Lean source.

Each proposal is wrapped in its own namespace `Cmh.Def.<name>.v<N>` together with a
fixed-type entry declaration `__cmh_entry`, elaborated by the Lean frontend against a
scratch copy of the base environment (`Init` + `Lean.Data.Json`), and the entry is then
turned into a native closure with `Environment.evalConst`. The kernel never elaborates
into a shared live environment: every version lives in its own environment value.
-/
open Lean Elab

namespace Cmh

/-- A userspace definition turned into something the kernel can call. -/
inductive Compiled where
  | tool (f : Json → IO String)
  | compose (f : List String → List String → String)
  | select (f : String → List (Nat × String × Int) → List Nat)
  | note

instance : Inhabited Compiled := ⟨.note⟩

/-- Modules available to userspace source (no `import` lines are allowed in a proposal). -/
def userImports : Array Import := #[{ module := `Init }, { module := `Lean.Data.Json }]

initialize baseEnvRef : IO.Ref (Option Environment) ← IO.mkRef none
/-- Environments whose compiled entries are in use; never dropped during the process. -/
initialize keepAlive : IO.Ref (Array Environment) ← IO.mkRef #[]

unsafe def enableInitsImpl : IO Unit := enableInitializersExecution
@[implemented_by enableInitsImpl] opaque enableInits : IO Unit

/-- Toolchain root holding the `.olean` files: `LEAN_SYSROOT`, `lean --print-prefix`, or the
pinned elan toolchain directory. -/
def leanSysroot : IO System.FilePath := do
  try
    Lean.findSysroot
  catch _ =>
    let home := (← IO.getEnv "HOME").getD "/root"
    let elanHome := (← IO.getEnv "ELAN_HOME").getD (home ++ "/.elan")
    return System.FilePath.mk elanHome / "toolchains" / s!"leanprover--lean4---v{Lean.versionString}"

/-- The base environment, imported once per process on first use. -/
def getBaseEnv : IO Environment := do
  if let some e ← baseEnvRef.get then return e
  enableInits
  Lean.initSearchPath (← leanSysroot)
  let env ← importModules userImports {} (loadExts := true)
  baseEnvRef.set (some env)
  return env

/-- What a definition of one (kind, name) must provide. -/
structure Contract where
  /-- The declaration the source must define, as shown to the model on elaboration errors. -/
  signature : String
  /-- The fixed entry declaration appended after the source; it type-checks the contract. -/
  entry : String

/-- The contract table. Notes have none. `evalEntryImpl` must evaluate each entry at the type
written here, and the `propose_def` tool description repeats the signatures. -/
def contract? (kind name : String) : Option Contract :=
  match kind, name with
  | "tool", _ => some {
      signature := "def run (input : Lean.Json) : IO String"
      entry := "def __cmh_entry : Lean.Json → IO String := run" }
  | "hook", "compose_system" => some {
      signature := "def composeSystem (notes lessons : List String) : String"
      entry := "def __cmh_entry : List String → List String → String := composeSystem" }
  | "hook", "select_lessons" => some {
      signature := "def selectLessons (task : String) (lessons : List (Nat × String × Int)) : List Nat"
      entry := "def __cmh_entry : String → List (Nat × String × Int) → List Nat := selectLessons" }
  | _, _ => none

/-- Tokens whose presence rejects a proposal before elaboration (Lean-specific gate rule). -/
def bannedWords : List String :=
  ["sorry", "sorryAx", "axiom", "implemented_by", "unsafe", "extern", "#eval", "#exit",
   "import", "initialize", "builtin_initialize", "#print"]

def bannedIn (src : String) : List String :=
  let toks := src.splitToList (fun c => !(c.isAlphanum || c == '_' || c == '#'))
  bannedWords.filter (toks.contains ·)

/-- One case per row of `contract?`, at that row's entry type. -/
unsafe def evalEntryImpl (kind name : String) (env : Environment) (n : Name) : Except String Compiled :=
  match kind, name with
  | "tool", _ => (env.evalConst (Json → IO String) {} n (checkMeta := false)).map .tool
  | "hook", "compose_system" =>
    (env.evalConst (List String → List String → String) {} n (checkMeta := false)).map .compose
  | "hook", "select_lessons" =>
    (env.evalConst (String → List (Nat × String × Int) → List Nat) {} n (checkMeta := false)).map .select
  | _, _ => .error s!"no entry point for kind {kind}"

/-- `evalConst` at the entry's known type. Only called on declarations we generated with that type. -/
@[implemented_by evalEntryImpl]
opaque evalEntry (kind name : String) (env : Environment) (n : Name) : Except String Compiled

private def fmtMessage (m : Message) : IO String := do
  let sev := match m.severity with
    | .error => "error" | .warning => "warning" | .information => "info"
  -- line 1 of the wrapped file is our `namespace` line, so shift back by one
  let line := m.pos.line - 1
  return s!"line {line}:{m.pos.column}: {sev}: {(← m.data.toString).trimAscii.toString}"

/-- Elaborate `source` as version `version` of `name` in a scratch environment.
Returns the compiled entry or an error text containing the elaborator's messages. -/
def elaborate (name kind : String) (version : Nat) (source : String) (maxHeartbeats : Nat := 200000) :
    IO (Except String Compiled) := do
  if kind == "note" then return .ok .note
  let some contract := contract? kind name | return .error s!"unsupported kind/name {kind}/{name}"
  let banned := bannedIn source
  unless banned.isEmpty do
    return .error s!"rejected before elaboration: source contains forbidden token(s) {banned} \
      (sorry/axiom/unsafe/implemented_by/extern/#eval/import/initialize are not allowed)"
  let ns := s!"Cmh.Def.{name}.v{version}"
  let full := s!"namespace {ns}\n{source}\n{contract.entry}\nend {ns}\n"
  let opts : Options := (Options.empty.set `maxHeartbeats maxHeartbeats).set `Elab.async false
  try
    let env ← getBaseEnv
    let (env', msgs) ← Lean.Elab.process full env opts (fileName := s!"{name}.v{version}.lean")
    -- errors first: they are what the model needs to fix
    let (errs, others) := msgs.toList.partition (·.severity == .error)
    let rendered ← (errs ++ others).mapM fmtMessage
    let usesSorry := rendered.any (·.contains "sorry")
    if msgs.hasErrors || usesSorry then
      return .error s!"elaboration failed:\n{String.intercalate "\n" rendered}\n\
        (contract: {contract.signature}; theorems/examples must check; \
        the last source line is followed by the kernel's entry declaration)"
    match evalEntry kind name env' s!"{ns}.__cmh_entry".toName with
    | .ok c =>
      -- closures produced by the interpreter refer to declarations of `env'`; keep it alive
      keepAlive.modify (·.push env')
      return .ok c
    | .error e => return .error s!"evalConst failed: {e}"
  catch e =>
    return .error s!"elaboration raised: {e}"

/-- Run `act` on a dedicated thread with a wall-clock limit, converting exceptions to `.error`.
On timeout the thread cannot be killed (Lean tasks are cooperative); it is abandoned. -/
def runTimed (ms : Nat) (act : IO α) : IO (Except String α) := do
  let t ← IO.asTask act .dedicated
  let start ← IO.monoMsNow
  let mut delay : UInt32 := 1
  repeat
    if ← IO.hasFinished t then break
    if (← IO.monoMsNow) - start > ms then
      IO.cancel t
      return .error s!"timeout after {ms / 1000}s"
    IO.sleep delay
    delay := min 20 (delay * 2)
  match t.get with
  | .ok v => return .ok v
  | .error e => return .error (toString e)

end Cmh
