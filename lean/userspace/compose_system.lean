/-- One markdown section per non-empty list. -/
def mkSection (title : String) (items : List String) : List String :=
  if items.isEmpty then []
  else ["## " ++ title ++ "\n" ++ String.intercalate "\n" (items.map ("- " ++ ·))]

def composeSystem (notes lessons : List String) : String :=
  String.intercalate "\n\n" (mkSection "Notes" notes ++ mkSection "Lessons" lessons)
