import Cmh

/-- Exit through `IO.Process.exit` so a userspace call abandoned after a timeout (Lean threads
cannot be killed) does not keep the process alive at shutdown. -/
def main (args : List String) : IO UInt32 := do
  let code ← Cmh.cliMain args
  (← IO.getStdout).flush
  (← IO.getStderr).flush
  IO.Process.exit code.toUInt8
