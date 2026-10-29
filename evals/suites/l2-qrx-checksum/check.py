import os, sys
sys.path.insert(0, sys.argv[1])
from setup import blobs, qrx, EPISODES
ep = int(os.environ.get("EVAL_EPISODE", "1"))
b = blobs()
files = EPISODES[ep - 1]
lines = open("answer.txt").read().strip().splitlines()
if ep < 4:
    assert lines == [qrx(b[files[0]])], (lines, qrx(b[files[0]]))
else:
    want = [f"{n} {qrx(b[n])}" for n in sorted(files)]
    assert [" ".join(l.split()) for l in lines] == want, (lines, want)
print("pass")
