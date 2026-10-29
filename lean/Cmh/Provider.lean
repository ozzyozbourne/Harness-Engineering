import Cmh.Util
/-!
Model providers: Anthropic Messages API (through a `curl` subprocess) and a scripted mock.
-/
open Lean

namespace Cmh

structure Request where
  /-- System blocks chosen by the kernel: base prompt first, then the supplemental prompt. -/
  system : Array String
  tools : Array Json := #[]
  messages : Array Json

def Request.toJson (r : Request) : Json :=
  Json.mkObj [("system", Lean.toJson r.system), ("tools", Lean.toJson r.tools), ("messages", Lean.toJson r.messages)]

structure Response where
  content : Array Json
  stopReason : String

structure Provider where
  name : String
  complete : Request → IO Response

def oauthPreamble : String := "You are Claude Code, Anthropic's official CLI for Claude."

/-! ## Mock -/

/-- Every request the mock provider received in this process, in order. -/
initialize mockRequests : IO.Ref (Array Json) ← IO.mkRef #[]

def mockProvider (script : Array Json) : IO Provider := do
  let idx ← IO.mkRef 0
  return {
    name := "mock"
    complete := fun req => do
      mockRequests.modify (·.push req.toJson)
      let i ← idx.modifyGet fun i => (i, i + 1)
      let some content := script[i]? |
        throw (IO.userError s!"mock provider: script exhausted (request {i + 1}, script has {script.size})")
      let blocks := match content with | .arr a => a | j => #[j]
      let stop := if blocks.any (fun b => jStr b "type" == "tool_use") then "tool_use" else "end_turn"
      return { content := blocks, stopReason := stop } }

def mockProviderFromFile (path : System.FilePath) : IO Provider := do
  match Json.parse (← IO.FS.readFile path) with
  | .ok (.arr a) => mockProvider a
  | .ok _ => throw (IO.userError "mock script must be a JSON array of content arrays")
  | .error e => throw (IO.userError s!"mock script: {e}")

/-! ## Anthropic -/

private def runCurl (url bodyPath respPath : String) (config : String) : IO (UInt32 × String × String) := do
  let child ← IO.Process.spawn {
    cmd := "curl"
    args := #["-sS", "-X", "POST", url, "-K", "-", "--data-binary", s!"@{bodyPath}",
              "-o", respPath, "-w", "%{http_code}", "--max-time", "600"]
    stdin := .piped, stdout := .piped, stderr := .piped }
  let (stdin, child) ← child.takeStdin
  stdin.putStr config
  stdin.flush  -- last use: the handle is released (closed) here, so curl sees EOF
  let out ← child.stdout.readToEnd
  let err ← child.stderr.readToEnd
  let code ← child.wait
  return (code, strTrim out, strTrim err)

/-- Anthropic Messages API. Auth: `ANTHROPIC_API_KEY` (x-api-key) or `CLAUDE_CODE_OAUTH_TOKEN`
(Bearer + oauth beta header + Claude Code system preamble). Headers, including the secret, go to
curl through a config file on stdin, never argv. Honors `ANTHROPIC_BASE_URL` (http or https). -/
def anthropicProvider (model : String) : IO Provider := do
  let apiKey := (← IO.getEnv "ANTHROPIC_API_KEY").filter (· != "")
  let oauth := (← IO.getEnv "CLAUDE_CODE_OAUTH_TOKEN").filter (· != "")
  if apiKey.isNone && oauth.isNone then
    throw (IO.userError "anthropic provider: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN")
  let base := ((← IO.getEnv "ANTHROPIC_BASE_URL").filter (· != "")).getD "https://api.anthropic.com"
  let base := if base.endsWith "/" then (base.dropEnd 1).toString else base
  let url := base ++ "/v1/messages"
  let authLines := match apiKey with
    | some k => s!"header = \"x-api-key: {k}\"\n"
    | none => s!"header = \"authorization: Bearer {oauth.getD ""}\"\nheader = \"anthropic-beta: oauth-2025-04-20\"\n"
  let config := "header = \"content-type: application/json\"\nheader = \"anthropic-version: 2023-06-01\"\n" ++ authLines
  return {
    name := "anthropic"
    complete := fun req => do
      let sys := (if apiKey.isNone then #[oauthPreamble] else #[]) ++ req.system.filter (· != "")
      let mut fields : List (String × Json) := [("model", model), ("max_tokens", (32000 : Nat)),
        ("system", Lean.toJson (sys.map fun t => Json.mkObj [("type", "text"), ("text", Json.str t)])),
        ("messages", Lean.toJson req.messages)]
      unless req.tools.isEmpty do fields := fields ++ [("tools", Lean.toJson req.tools)]
      let body := (Json.mkObj fields).compress
      let (bh, bodyPath) ← IO.FS.createTempFile
      bh.putStr body; bh.flush
      let (_, respPath) ← IO.FS.createTempFile
      try
        let mut attempt := 0
        repeat
          let (code, http, err) ← runCurl url bodyPath.toString respPath.toString config
          let resp ← IO.FS.readFile respPath <|> pure ""
          let status := http.toNat?.getD 0
          let retryable := code != 0 || status == 429 || status == 529 || status ≥ 500
          if retryable && attempt < 4 then
            attempt := attempt + 1
            IO.eprintln s!"[provider] retry {attempt} after curl exit {code} http {http} {strTake err 200}"
            IO.sleep (UInt32.ofNat (2000 * 2 ^ attempt))
            continue
          if code != 0 then throw (IO.userError s!"curl failed (exit {code}): {err}")
          match Json.parse resp with
          | .error e => throw (IO.userError s!"http {http}: unparsable response ({e}): {strTake resp 500}")
          | .ok j =>
            if status != 200 then
              throw (IO.userError s!"http {http}: {strTake (jVal j "error").compress 800}")
            return { content := jArr j "content", stopReason := jStr j "stop_reason" }
        throw (IO.userError "unreachable")
      finally
        try IO.FS.removeFile bodyPath catch _ => pure ()
        try IO.FS.removeFile respPath catch _ => pure () }

end Cmh
