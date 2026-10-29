import sys
sys.path.insert(0, sys.argv[1])
from setup import blobs, qrx, EPISODES, episode
b = blobs()
files = EPISODES[episode() - 1]
lines = open("answer.txt").read().strip().splitlines()
if len(files) == 1:  # single file: just the checksum
    assert lines == [qrx(b[files[0]])], (lines, qrx(b[files[0]]))
else:
    want = [f"{n} {qrx(b[n])}" for n in sorted(files)]
    assert [" ".join(l.split()) for l in lines] == want, (lines, want)
print("pass")
