#!/usr/bin/env python3
"""Map old line numbers (at base commit) to new line numbers (HEAD) per file.

Builds the map from `git diff -U0 BASE HEAD -- file`. Lines inside changed
hunks are flagged MODIFIED (need manual re-derivation).
"""
import re
import subprocess
import sys

# features/02's original anchors match 1f257f1 (#564), NOT 544afd2 (#581):
# the only file differing between those trees is viewer.html, so 01/03
# (index.html, flamegraph.js, ...) remaps done with 544afd2 were unaffected.
BASE = "1f257f1"
REPO = "/Users/facundo/code/wye/dial9-tokio-telemetry/.claude/worktrees/T01"


def hunks(path):
    out = subprocess.run(
        ["git", "diff", "-U0", BASE, "HEAD", "--", f"dial9-viewer/ui/{path}"],
        cwd=REPO, capture_output=True, text=True, check=True).stdout
    hs = []
    for m in re.finditer(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", out, re.M):
        a, alen, b, blen = (int(m.group(1)), int(m.group(2) or "1"),
                            int(m.group(3)), int(m.group(4) or "1"))
        hs.append((a, alen, b, blen))
    return hs


def make_mapper(path):
    hs = hunks(path)

    def mapper(old):
        delta = 0
        for a, alen, b, blen in hs:
            # for pure insertions (alen==0), hunk position a means "after line a"
            if alen == 0:
                if old <= a:
                    break
                delta += blen
            else:
                if old < a:
                    break
                if old < a + alen:
                    return None  # inside a modified/deleted region
                delta += blen - alen
        return old + delta

    return mapper


MAPPERS = {}


def map_line(path, old):
    if path not in MAPPERS:
        MAPPERS[path] = make_mapper(path)
    return MAPPERS[path](old)


if __name__ == "__main__":
    path = sys.argv[1]
    for arg in sys.argv[2:]:
        if "-" in arg:
            a, b = arg.split("-")
            print(f"{arg} -> {map_line(path, int(a))}-{map_line(path, int(b))}")
        else:
            print(f"{arg} -> {map_line(path, int(arg))}")
