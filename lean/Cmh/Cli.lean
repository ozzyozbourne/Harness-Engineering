import Cmh.Kernel
/-! Command-line interface. -/
open Lean

namespace Cmh

def usage : String :=
  "usage: cmh [--state DIR] [--provider anthropic|mock] [--script FILE] [--model M]
           [--soak N] [--no-reflect] <command>
  repl            interactive; slash commands: /defs /show NAME [VERSION] /knowledge /refine
                  /rollback NAME VERSION /quit
  run \"TASK\"      one task, prints the final answer, exits 0 on success
  refine          one refine task
  defs            print the registry
  knowledge       print lessons and verdicts"

structure Args where
  state : String := ".cmh"
  provider : String := "anthropic"
  script : Option String := none
  model : Option String := none
  soak : Nat := 3
  reflect : Bool := true
  rest : List String := []

partial def parseArgs : List String → Args → Except String Args
  | "--state" :: v :: r, a => parseArgs r { a with state := v }
  | "--provider" :: v :: r, a => parseArgs r { a with provider := v }
  | "--script" :: v :: r, a => parseArgs r { a with script := some v }
  | "--model" :: v :: r, a => parseArgs r { a with model := some v }
  | "--soak" :: v :: r, a => match v.toNat? with
    | some n => parseArgs r { a with soak := n }
    | none => .error "--soak needs a number"
  | "--no-reflect" :: r, a => parseArgs r { a with reflect := false }
  | x :: r, a => if x.startsWith "--" && a.rest.isEmpty then .error s!"unknown option {x}"
                 else parseArgs r { a with rest := a.rest ++ [x] }
  | [], a => .ok a

def mkProvider (a : Args) : IO Provider := do
  match a.provider with
  | "mock" =>
    let some f := a.script | throw (IO.userError "--provider mock needs --script FILE")
    mockProviderFromFile f
  | "anthropic" =>
    let model := a.model.getD ((← envNonEmpty? "CMH_MODEL").getD "claude-sonnet-5-5")
    anthropicProvider model
  | p => throw (IO.userError s!"unknown provider {p}")

/-- Run one task, print its final answer (if any), and return the exit code: 0 on success. -/
def runAndReport (k : Kernel) (task : String) : IO UInt32 := do
  let o ← k.runTask task
  if o.answer != "" then IO.println o.answer
  return if o.success then 0 else 1

partial def repl (k : Kernel) : IO Unit := do
  let stdin ← IO.getStdin
  repeat
    IO.print "cmh> "
    (← IO.getStdout).flush
    let line ← stdin.getLine
    if line.isEmpty then break
    let line := strTrim line
    if line.isEmpty then continue
    match line.splitOn " " |>.filter (· != "") with
    | ["/quit"] => break
    | ["/defs"] => IO.println (← k.listDefs)
    | ["/knowledge"] => IO.println (← k.knowledgeText)
    | ["/show", n] => IO.println (← k.showDef n none).1
    | ["/show", n, v] => IO.println (← k.showDef n v.toNat?).1
    | ["/rollback", n, v] => IO.println (← k.rollback n (v.toNat?.getD 0)).1
    | ["/refine"] => discard <| k.runTask (← k.refinePrompt)
    | w :: _ =>
      if w.startsWith "/" then IO.println s!"unknown command {w}"
      else discard <| k.runTask line
    | [] => pure ()

/-- Entry point shared by `main` and the tests. Returns the exit code. -/
def cliMain (argv : List String) : IO UInt32 := do
  match parseArgs argv {} with
  | .error e => IO.eprintln e; IO.eprintln usage; return 2
  | .ok a =>
  let needsModel := match a.rest with | "run" :: _ | ["refine"] | ["repl"] => true | _ => false
  try
    let provider ← if needsModel then mkProvider a
      else pure { complete := fun _ => throw (IO.userError "no provider") }
    let isRepl := a.rest == ["repl"]
    let k ← Kernel.open { stateDir := a.state, workDir := ← IO.currentDir, soak := a.soak,
                          reflect := a.reflect, echo := isRepl } provider
    match a.rest with
    | ["run", task] => runAndReport k task
    | ["refine"] => runAndReport k (← k.refinePrompt)
    | ["repl"] => repl k; return 0
    | ["defs"] => IO.println (← k.listDefs); return 0
    | ["knowledge"] => IO.println (← k.knowledgeText); return 0
    | _ => IO.eprintln usage; return 2
  catch e =>
    IO.eprintln s!"cmh: {e}"
    return 1

end Cmh
