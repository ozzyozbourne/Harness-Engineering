/-- Lower-cased words of 4 or more letters. -/
def longWords (s : String) : List String :=
  (s.toLower.splitToList (fun c => !c.isAlpha)).filter (·.length ≥ 4)

/-- The 8 highest-scored lessons sharing a 4+ letter word with the task, else the top 3. -/
def selectLessons (task : String) (lessons : List (Nat × String × Int)) : List Nat :=
  let tw := longWords task
  let sorted := lessons.mergeSort (fun a b => decide (a.2.2 ≥ b.2.2))
  let matching := sorted.filter (fun l => (longWords l.2.1).any (tw.contains ·))
  (if matching.isEmpty then sorted.take 3 else matching.take 8).map (·.1)
