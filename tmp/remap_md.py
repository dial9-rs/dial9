#!/usr/bin/env python3
"""Rewrite line anchors in an inventory md from base-commit to HEAD coords.

Rules:
- backtick spans of the form `FILE:RANGES` (FILE in KNOWN) remap via FILE's map
- backtick spans that are ONLY ranges (`123`, `123-456`, `1,2-3`) remap via the
  md file's default source file
- endpoints inside changed hunks -> left as-is, WARN emitted (row needs manual
  attention because its code changed)
- trace_parser.js is force-manual (whole-file reindent): always WARN
"""
import re
import sys

sys.path.insert(0, "/Users/facundo/code/wye/dial9-tokio-telemetry/.claude/worktrees/T01/tmp")
from linemap import map_line

KNOWN = ["index.html", "viewer.html", "flamegraph.html", "tokio_stats.html",
         "flamegraph.js", "trace_analysis.js", "trace_parser.js", "heatmap.js",
         "creds.js", "prefix_detect.js", "panel_layout.js", "format.js",
         "decode.js", "flamegraph_export.js", "flamegraph.css", "url_state.js"]
MANUAL = {"trace_parser.js"}

warns = []


def remap_ranges(fname, ranges_str, ctx):
    """ranges_str like '123-456,789' -> remapped string or None on any failure."""
    out = []
    ok = True
    for part in ranges_str.split(","):
        part = part.strip()
        m = re.fullmatch(r"(\d+)(?:-(\d+))?", part)
        if not m:
            return None
        a = int(m.group(1))
        b = int(m.group(2)) if m.group(2) else None
        if fname in MANUAL:
            warns.append(f"MANUAL {fname}:{part} :: {ctx}")
            ok = False
            out.append(part)
            continue
        na = map_line(fname, a)
        nb = map_line(fname, b) if b is not None else None
        if na is None or (b is not None and nb is None):
            warns.append(f"TOUCHED {fname}:{part} :: {ctx}")
            ok = False
            out.append(part)
            continue
        out.append(f"{na}-{nb}" if b is not None else f"{na}")
    return ",".join(out) if ok else None


def process(md_path, default_file):
    text = open(md_path).read()
    lines = text.split("\n")
    out_lines = []
    file_alt = "|".join(re.escape(k) for k in KNOWN)

    for ln_no, line in enumerate(lines, 1):
        def sub_span(m):
            content = m.group(1)
            ctx = f"{md_path.split('/')[-1]}:{ln_no}"
            # `FILE:RANGES`
            fm = re.fullmatch(rf"({file_alt}):([\d,\-]+)", content)
            if fm:
                new = remap_ranges(fm.group(1), fm.group(2), ctx)
                return f"`{fm.group(1)}:{new}`" if new else m.group(0)
            # bare ranges only
            if re.fullmatch(r"\d+(?:-\d+)?(?:,\s*\d+(?:-\d+)?)*", content):
                new = remap_ranges(default_file, content, ctx)
                return f"`{new}`" if new else m.group(0)
            return m.group(0)

        out_lines.append(re.sub(r"`([^`]+)`", sub_span, line))

    open(md_path, "w").write("\n".join(out_lines))


if __name__ == "__main__":
    process(sys.argv[1], sys.argv[2])
    print("WARNINGS (need manual attention):")
    for w in warns:
        print(" ", w)
