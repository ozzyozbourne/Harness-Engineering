import Lake
open Lake DSL

package cmh where
  testDriver := "«cmh-test»"

lean_lib Cmh

@[default_target]
lean_exe cmh where
  root := `Main
  supportInterpreter := true

lean_exe «cmh-test» where
  root := `Test
  supportInterpreter := true
