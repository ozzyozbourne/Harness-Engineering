import Lean.Data.Json
import Std.Time
/-! Small helpers shared by the kernel: strings, JSON access, time. -/
open Lean

namespace Cmh

def strTrim (s : String) : String := s.trimAscii.toString

def strTake (s : String) (n : Nat) : String := (s.take n).toString

/-- `true` if `sub` occurs in `s`. -/
def hasSub (s sub : String) : Bool :=
  sub.isEmpty || (s.splitOn sub).length > 1

/-- Truncate to at most `n` bytes-ish (characters), marking the cut. -/
def truncate (s : String) (n : Nat) : String :=
  if s.length ≤ n then s else strTake s n ++ s!"\n... [truncated, {s.length} chars total]"

/-- Lower-cased alphanumeric words. -/
def words (s : String) : List String :=
  (s.toLower.splitToList (fun c => !c.isAlphanum)).filter (!·.isEmpty)

/-- Lower-cased words of 4+ letters (the seed `select_lessons` rule). -/
def longWords (s : String) : List String :=
  (s.toLower.splitToList (fun c => !c.isAlpha)).filter (·.length ≥ 4)

def jStr (j : Json) (k : String) (d : String := "") : String :=
  (j.getObjValAs? String k).toOption.getD d

def jStr? (j : Json) (k : String) : Option String :=
  (j.getObjValAs? String k).toOption

def jNat? (j : Json) (k : String) : Option Nat :=
  match j.getObjVal? k with
  | .ok (.num n) => if n.exponent == 0 && n.mantissa ≥ 0 then some n.mantissa.toNat else none
  | .ok (.str s) => s.toNat?
  | _ => none

def jInt (j : Json) (k : String) (d : Int := 0) : Int :=
  (j.getObjValAs? Int k).toOption.getD d

def jBool (j : Json) (k : String) (d : Bool := false) : Bool :=
  (j.getObjValAs? Bool k).toOption.getD d

def jVal (j : Json) (k : String) : Json :=
  (j.getObjVal? k).toOption.getD Json.null

def jArr (j : Json) (k : String) : Array Json :=
  match j.getObjVal? k with
  | .ok (.arr a) => a
  | _ => #[]

/-- Civil date from days since 1970-01-01 (Howard Hinnant's algorithm). -/
private def civil (z : Int) : Int × Nat × Nat :=
  let z := z + 719468
  let era := (if z ≥ 0 then z else z - 146096) / 146097
  let doe := z - era * 146097
  let yoe := (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365
  let y := yoe + era * 400
  let doy := doe - (365 * yoe + yoe / 4 - yoe / 100)
  let mp := (5 * doy + 2) / 153
  let d := doy - (153 * mp + 2) / 5 + 1
  let m := if mp < 10 then mp + 3 else mp - 9
  ((if m ≤ 2 then y + 1 else y), m.toNat, d.toNat)

private def pad2 (n : Nat) : String := if n < 10 then s!"0{n}" else toString n

/-- Current UTC time as ISO-8601, e.g. `2026-10-29T10:40:00Z`. -/
def nowIso : IO String := do
  let t ← Std.Time.Timestamp.now
  let secs : Int := t.toSecondsSinceUnixEpoch.val
  let days := secs / 86400
  let rem := (secs - days * 86400).toNat
  let (y, m, d) := civil days
  return s!"{y}-{pad2 m}-{pad2 d}T{pad2 (rem / 3600)}:{pad2 (rem % 3600 / 60)}:{pad2 (rem % 60)}Z"

end Cmh
