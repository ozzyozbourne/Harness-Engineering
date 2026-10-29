/-- Number of maximal runs of non-whitespace characters. -/
def countWords (s : String) : Nat :=
  (s.toList.foldl (fun (acc : Nat × Bool) c =>
    if c.isWhitespace then (acc.1, false)
    else if acc.2 then acc else (acc.1 + 1, true)) (0, false)).1

theorem countWords_empty : countWords "" = 0 := rfl

def run (input : Lean.Json) : IO String := do
  let text := (input.getObjValAs? String "text").toOption.getD ""
  pure (toString (countWords text))
