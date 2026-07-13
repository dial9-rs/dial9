"use strict";

// Pure helpers for the two-sided differential flamegraph (the `diff=1` branch
// of flamegraph.html). Kept DOM-free and CommonJS-exported so the whole diff
// core — tree merge, color mapping, and the scope-link codec — is unit-testable
// under Node. In the browser these attach as globals via the top-level
// `function` declarations.
//
// The comparison is "two-sided": side A is drawn on the left, side B on the
// right. Each side's box widths are normalized to its OWN total, so a frame
// huge in one capture and tiny in the other shows at its true proportion in
// each panel. A box's COLOR encodes relative hotness — blue = heavier in A,
// red = heavier in B, grey at parity — and is identical in both panels (only
// the width differs). This mirrors the standalone two-sided viewer shipped by
// the dial9-flamegraph-diff skill; the color/normalization math is the same.

// ---------------------------------------------------------------------------
// Tree merge
// ---------------------------------------------------------------------------

// A merged diff node carries BOTH sides' counts:
//   { name, a, b, selfA, selfB, children: Map<name, node> }
// `a`/`b` are total samples flowing through the node in each capture;
// `selfA`/`selfB` are leaf (self) samples. `children` is a Map keyed by frame
// name, matching the shape the canvas renderer already traverses.
function newDiffNode(name) {
  return { name: name, a: 0, b: 0, selfA: 0, selfB: 0, children: new Map() };
}

// Accumulate one side's server JSON tree (`{name, count, self, children[]}`,
// the FlamegraphNode wire shape) into the merged tree, keyed by frame name at
// each depth. `side` is "a" or "b". A null/missing tree contributes nothing
// (e.g. one side still loading, or a scope that matched no samples).
function addSide(mergedNode, jsonNode, side) {
  if (!jsonNode) return;
  const totKey = side === "a" ? "a" : "b";
  const selfKey = side === "a" ? "selfA" : "selfB";
  mergedNode[totKey] += jsonNode.count || 0;
  // The wire field is `self` (serde rename of self_count); omitted when 0.
  mergedNode[selfKey] += jsonNode.self || 0;
  const kids = jsonNode.children || [];
  for (let i = 0; i < kids.length; i++) {
    const child = kids[i];
    let m = mergedNode.children.get(child.name);
    if (!m) {
      m = newDiffNode(child.name);
      mergedNode.children.set(child.name, m);
    }
    addSide(m, child, side);
  }
}

// Merge two server JSON flamegraph trees (either may be null) into one diff
// tree. The root's `a`/`b` are the two capture totals (the server root "(all)"
// node's count is the total sample count), so callers can read totals straight
// off the merged root without threading them separately.
function mergeTrees(treeA, treeB) {
  const name = (treeA && treeA.name) || (treeB && treeB.name) || "(all)";
  const root = newDiffNode(name);
  addSide(root, treeA, "a");
  addSide(root, treeB, "b");
  return root;
}

// ---------------------------------------------------------------------------
// Color: relative hotness
// ---------------------------------------------------------------------------

// Map a node's two counts to an `rgb(...)` string. score > 0 => heavier in B
// (red), score < 0 => heavier in A (blue), 0 => grey (rgb 207,207,207). Based
// on each side's self-normalized fraction so differing totals need no separate
// normalize pass. Scale: +/-4 octaves of the B/A fraction ratio clamps to full
// saturation.
//
// A single SHARED epsilon floor keeps the ratio finite when a frame is present
// on only one side. It must be smaller than the smallest real single-sample
// fraction on EITHER side so a one-sided frame always tilts toward the side it
// appears on — `0.5 / max(tA, tB)` is below both `1/tA` and `1/tB`, so it does.
// A per-side floor (`0.5/tA` vs `0.5/tB`) is NOT comparable across sides: when
// the totals differ wildly (e.g. a 1-sample baseline vs an 8846-sample
// incident) the larger floor swamps the other side's real fraction, and a frame
// unique to the bigger side colors backwards (blue — "heavier in A" — even
// though it never appears in A).
function diffColor(a, b, totalA, totalB) {
  const tA = totalA || 1;
  const tB = totalB || 1;
  const eps = 0.5 / Math.max(tA, tB);
  const fa = a / tA + eps;
  const fb = b / tB + eps;
  let s = Math.log2(fb / fa) / 4; // +/-4 octaves -> +/-1
  if (s > 1) s = 1;
  else if (s < -1) s = -1;
  const t = Math.abs(s);
  if (s >= 0) {
    // heavier in B -> red
    const g = Math.round(207 - 167 * t);
    const bl = Math.round(207 - 191 * t);
    return "rgb(" + Math.round(207 + 48 * t) + "," + g + "," + bl + ")";
  }
  // heavier in A -> blue
  const r = Math.round(207 - 148 * t);
  const g = Math.round(207 - 80 * t);
  return "rgb(" + r + "," + g + "," + Math.round(207 + 48 * t) + ")";
}

// ---------------------------------------------------------------------------
// Per-side layout
// ---------------------------------------------------------------------------

// Lay out one side of the diff as a flat list of boxes for rendering. Widths
// are normalized to the ZOOM ROOT's own count on that side (`focus[side]`), so
// each panel reads as that capture's own flamegraph at true proportions; the
// two panels share color (diffColor) but not width. Mirrors renderSide in the
// dial9-flamegraph-diff skill's build_twosided.py, adapted to the merged
// Map-tree.
//
//   focus     — the merged node at the current zoom root
//   focusPath — array of frame names from "(all)" down to focus (inclusive)
//   side      — "a" or "b"; selects which count drives width
//   widthPx   — pixel width of the panel
//   minPx     — sub-pixel cutoff; boxes narrower than this are skipped (still in
//               the data, just not drawn). Default 0.4.
//
// Returns { boxes: [{ name, depth, x, w, a, b, selfA, selfB, path }], maxDepth }.
// `x`/`w` are in pixels; `path` is the full frame-name path (used as a stable
// key for cross-panel highlight and co-zoom). Children are laid out left to
// right ordered by THIS side's value, so the dominant frames sit leftmost.
function layoutSide(focus, focusPath, side, widthPx, minPx) {
  const min = minPx != null ? minPx : 0.4;
  const valOf = side === "a" ? (n) => n.a : (n) => n.b;
  const denom = valOf(focus) || 1;
  const boxes = [];
  let maxDepth = 0;

  // Iterative DFS over [node, depth, xPx, path].
  const stack = [[focus, 0, 0, focusPath]];
  while (stack.length) {
    const frame = stack.pop();
    const node = frame[0];
    const depth = frame[1];
    const x = frame[2];
    const path = frame[3];
    const w = (valOf(node) / denom) * widthPx;
    if (w < min) continue; // sub-pixel: keep in data, don't draw
    if (depth > maxDepth) maxDepth = depth;
    boxes.push({
      name: node.name,
      depth: depth,
      x: x,
      w: w,
      a: node.a,
      b: node.b,
      selfA: node.selfA,
      selfB: node.selfB,
      path: path,
    });
    // Children ordered by this side's value (widest first). Pushed in reverse
    // so the widest is popped/laid out first (left-most).
    const kids = [...node.children.values()].sort((p, q) => valOf(q) - valOf(p));
    let cx = x;
    const placed = [];
    for (let i = 0; i < kids.length; i++) {
      const c = kids[i];
      placed.push([c, depth + 1, cx, path.concat(c.name)]);
      cx += (valOf(c) / denom) * widthPx;
    }
    for (let i = placed.length - 1; i >= 0; i--) stack.push(placed[i]);
  }
  return { boxes: boxes, maxDepth: maxDepth };
}

// Find a merged node by its frame-name path (["(all)", ...]). Returns null if
// the path doesn't exist (e.g. a zoom target present in one capture but pruned
// from the merged tree). The first path element is the root itself.
function nodeAtPath(root, path) {
  let n = root;
  for (let i = 1; i < path.length; i++) {
    n = n.children.get(path[i]);
    if (!n) return null;
  }
  return n;
}

// ---------------------------------------------------------------------------
// Scope-link codec
// ---------------------------------------------------------------------------

// The flamegraph tab's address bar is intentionally lossy (it drops bucket /
// prefix / max_files — see updateBrowserUrl in flamegraph.html), so a copied
// address-bar URL cannot refetch the aggregate. A "scope link" instead carries
// the COMPLETE set of params needed to re-run the server-side refinement loop.
// AWS credentials are NEVER part of a scope link.
//
// `params` is a URLSearchParams (or anything with get/getAll). Returns a fresh
// URLSearchParams containing only the allowlisted scope keys that are present.
const SCOPE_KEYS_SINGLE = [
  "api",
  "data_dir",
  "bucket",
  // The bucket's region (a property of the bucket, not a credential), so a
  // scope/diff link signs the right regional S3 endpoint on its own.
  "aws_region",
  "prefix",
  "service",
  "thread_class",
  "source",
  "spawn_location",
  "start_ns",
  "end_ns",
  "max_files",
];

function fullScopeQuery(params) {
  const out = new URLSearchParams();
  for (let i = 0; i < SCOPE_KEYS_SINGLE.length; i++) {
    const k = SCOPE_KEYS_SINGLE[i];
    const v = params.get(k);
    if (v != null && v !== "") out.set(k, v);
  }
  // `host` is repeatable (the selection's host set).
  const hosts = params.getAll("host");
  for (let i = 0; i < hosts.length; i++) out.append("host", hosts[i]);
  return out;
}

// ---------------------------------------------------------------------------
// Diff presets (issue #624): derive side B from side A without a second tab
// ---------------------------------------------------------------------------

// Backward time-shift deltas for the "same scope, earlier window" presets, in
// nanoseconds. Kept as BigInt: start_ns/end_ns are ~1.78e18, well above
// Number.MAX_SAFE_INTEGER (~9.0e15), so Number arithmetic would silently
// corrupt them.
const DIFF_SHIFT_1H = 3600000000000n;
const DIFF_SHIFT_24H = 86400000000000n;
const DIFF_SHIFT_7D = 604800000000000n;

// "Same time, different host": return a NEW URLSearchParams copy of `scope`
// with ALL existing host params removed and a single host=<host> appended.
// Everything else (bucket/prefix/service/start_ns/end_ns/…) is preserved. The
// input is not mutated.
function scopeWithHost(scope, host) {
  const p = new URLSearchParams(scope);
  p.delete("host");
  p.append("host", host);
  return p;
}

// "Same scope, earlier window": return a NEW URLSearchParams copy of `scope`
// with start_ns/end_ns each shifted back by `deltaNs` (a BigInt or a value
// BigInt() accepts). The window LENGTH is preserved. If the scope has no
// start_ns/end_ns there is no window to shift, so the copy is returned
// unchanged. Host/service/bucket are untouched. The input is not mutated.
function shiftScopeTime(scope, deltaNs) {
  const p = new URLSearchParams(scope);
  const start = p.get("start_ns");
  const end = p.get("end_ns");
  if (start != null && end != null) {
    const d = BigInt(deltaNs);
    p.set("start_ns", (BigInt(start) - d).toString());
    p.set("end_ns", (BigInt(end) - d).toString());
  }
  return p;
}

// base64url (no padding) of a UTF-8 string. Works in both the browser and Node.
// Scope queries are ASCII (URLSearchParams percent-encodes the rest), but we
// route through a UTF-8-safe path anyway so the codec is not input-fragile.
function b64urlEncode(str) {
  let b64;
  if (typeof Buffer !== "undefined") {
    b64 = Buffer.from(str, "utf8").toString("base64");
  } else {
    // Encode UTF-8 -> binary string -> base64.
    const bytes = new TextEncoder().encode(str);
    let bin = "";
    for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    b64 = btoa(bin);
  }
  return b64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function b64urlDecode(s) {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/");
  if (typeof Buffer !== "undefined") {
    return Buffer.from(b64, "base64").toString("utf8");
  }
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder().decode(bytes);
}

// Encode one side's scope (a query string or URLSearchParams) for embedding in
// a diff link's `a=`/`b=` param.
function encodeScope(query) {
  return b64urlEncode(typeof query === "string" ? query : query.toString());
}

// Inverse of encodeScope: a base64url blob back to a URLSearchParams of that
// side's scope.
function decodeScope(b64) {
  return new URLSearchParams(b64urlDecode(b64));
}

// Build the query string (no leading "?") for a diff view comparing two scopes.
// Callers prepend "flamegraph.html?". Each scope is a query string or
// URLSearchParams (typically from fullScopeQuery).
function diffSearch(scopeA, scopeB) {
  return "diff=1&a=" + encodeScope(scopeA) + "&b=" + encodeScope(scopeB);
}

// Parse a diff view's location.search. Returns { a, b } (each a URLSearchParams
// of that side's scope) when this is a diff view, or null otherwise.
function parseDiff(search) {
  const p = typeof search === "string" ? new URLSearchParams(search) : search;
  if (p.get("diff") !== "1") return null;
  const a = p.get("a");
  const b = p.get("b");
  if (!a || !b) return null;
  return { a: decodeScope(a), b: decodeScope(b) };
}

// ---------------------------------------------------------------------------
// Button routing: single scope vs captured A/B diff
// ---------------------------------------------------------------------------

// Decide which page + query string a viz button should open. This is the shared
// routing seam behind the landing page's "Flamegraph"/"Tokio Stats" buttons and
// the diff tray's launch buttons, so all of them agree on the single-vs-diff
// decision. `kind` is "flamegraph" or "tokio". When `hasDiff` is true (a full
// A/B diff has been captured) the target is the two-sided diff link
// (?diff=1&a=..&b=..) built from `diffA`/`diffB`; otherwise it is the caller's
// pre-built single-scope `singleQuery`. Pure/DOM-free so the routing is
// unit-testable without a browser.
//
// Flamegraph diff scopes carry the client-only `api=1` flag per side (each side
// hits /api/flamegraph in aggregate mode); tokio-stats does not use it.
function chooseTarget(kind, opts) {
  const page = kind === "tokio" ? "tokio_stats.html" : "flamegraph.html";
  if (opts && opts.hasDiff) {
    const withApi = (scope) => {
      if (kind !== "flamegraph") return scope;
      const s = new URLSearchParams(
        typeof scope === "string" ? scope : scope.toString(),
      );
      s.set("api", "1");
      return s;
    };
    return { page: page, search: diffSearch(withApi(opts.diffA), withApi(opts.diffB)) };
  }
  const single = opts && opts.singleQuery != null ? opts.singleQuery : "";
  return { page: page, search: typeof single === "string" ? single : single.toString() };
}

// ---------------------------------------------------------------------------
// Capture-tray state machine (in-page "Add to diff")
// ---------------------------------------------------------------------------

// The in-page "Add to diff" tray (flamegraph.html's aggregate toolbar, and the
// landing page's heatmap tray) captures two scopes — A (left) and B (right) —
// then launches a two-sided diff. State is `{ a, b }` where each side is a
// scope (URLSearchParams) or null. These three transitions keep the invariant
// "A fills before B" so there is never a B-without-A hole. Pure/DOM-free so the
// tray wiring is unit-testable without a browser.

// Capture one more scope: fill A first, then B; once both are set a further
// add replaces B (the most recent), so the user can keep re-picking the
// comparison side.
function addDiffCapture(state, scope) {
  const a = state && state.a ? state.a : null;
  const b = state && state.b ? state.b : null;
  if (!a) return { a: scope, b: b };
  return { a: a, b: scope };
}

// Swap which capture is A vs B. Only meaningful when both sides are set;
// swapping a lone side would move it into B and leave A empty, violating the
// "fill A first" invariant — so it's a no-op then.
function swapDiffCapture(state) {
  const a = state && state.a ? state.a : null;
  const b = state && state.b ? state.b : null;
  if (!a || !b) return { a: a, b: b };
  return { a: b, b: a };
}

// Remove one side. Clearing A promotes B to A so there is never a B-without-A
// hole (the codec fills A first); clearing B just drops it.
function removeDiffSide(state, side) {
  const a = state && state.a ? state.a : null;
  const b = state && state.b ? state.b : null;
  if (side === "a") return { a: b, b: null };
  return { a: a, b: null };
}

var FlamegraphDiff = {
  newDiffNode,
  addSide,
  mergeTrees,
  diffColor,
  layoutSide,
  nodeAtPath,
  fullScopeQuery,
  scopeWithHost,
  shiftScopeTime,
  DIFF_SHIFT_1H,
  DIFF_SHIFT_24H,
  DIFF_SHIFT_7D,
  b64urlEncode,
  b64urlDecode,
  encodeScope,
  decodeScope,
  diffSearch,
  parseDiff,
  chooseTarget,
  addDiffCapture,
  swapDiffCapture,
  removeDiffSide,
  SCOPE_KEYS_SINGLE,
};

if (typeof module !== "undefined" && module.exports) {
  module.exports = FlamegraphDiff;
} else if (typeof window !== "undefined") {
  // Browser: expose as a namespace so flamegraph_diff_view.js can find it
  // (the individual top-level functions are also globals, but the namespace is
  // the stable handle the view module uses).
  window.FlamegraphDiff = FlamegraphDiff;
}
