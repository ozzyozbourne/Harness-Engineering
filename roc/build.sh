#!/usr/bin/env bash
# Builds cmh (Roc flavor) inside a roc-lang/roc checkout and links it to roc/bin/cmh.
#
#   ./build.sh [ROC_CHECKOUT]       default: /home/user/workspace/repos-untrusted/roc
#
# Environment: CMH_OPTIMIZE (default Debug; a release mode needs more than 6 GB
# of RAM for the LLVM step and was OOM-killed in a 7.8 GB orb),
# CMH_JOBS (default 2), ZIG (default: zig on PATH, else ~/.local/bin/zig).
# The checkout is moved to the pinned commit; only its build.zig is changed
# (by patch-build.py, which can be re-run safely).
set -euo pipefail

PIN=a5c6d8bf5bcb299d160f9eb5d4f953914254bf45
HERE=$(cd "$(dirname "$0")" && pwd)
ROC=$(cd "${1:-/home/user/workspace/repos-untrusted/roc}" && pwd)
OPT=${CMH_OPTIMIZE:-Debug}
JOBS=${CMH_JOBS:-2}
ZIG=${ZIG:-$(command -v zig || echo "$HOME/.local/bin/zig")}

cd "$ROC"
if [ "$(git rev-parse HEAD)" != "$PIN" ]; then
    git checkout -q -- build.zig
    git cat-file -e "$PIN^{commit}" 2>/dev/null || git fetch -q origin "$PIN"
    git checkout -q "$PIN"
fi
python3 -I "$HERE/patch-build.py" "$ROC" "$HERE"
"$ZIG" build cmh -j"$JOBS" -Doptimize="$OPT"
mkdir -p "$HERE/bin"
ln -sfn "$ROC/zig-out/bin/cmh" "$HERE/bin/cmh"
echo "built $HERE/bin/cmh -> $ROC/zig-out/bin/cmh ($OPT)"
