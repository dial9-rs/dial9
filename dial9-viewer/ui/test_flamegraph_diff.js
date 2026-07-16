#!/usr/bin/env node
"use strict";

// Unit tests for the pure two-sided differential flamegraph helpers in
// flamegraph_diff.js: the tree merge (union by frame name, carrying both sides'
// total and self counts), the relative-hotness color mapping, and the
// scope-link codec (which carries the COMPLETE refetch scope — unlike the lossy
// address-bar URL — and never carries credentials). All DOM-free.

const {
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
  pollBandLabel,
  diffSearch,
  parseDiff,
  chooseTarget,
  addDiffCapture,
  swapDiffCapture,
  removeDiffSide,
} = require("./flamegraph_diff.js");

let passed = 0;
let failed = 0;

function assertEq(actual, expected, desc) {
  if (actual === expected) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

function assert(cond, desc) {
  assertEq(!!cond, true, desc);
}

// Server JSON wire shape: { name, count, self, children[] }. `self`/`children`
// are omitted when 0/empty (serde skip), so the merge must tolerate missing.
function node(name, count, self, children) {
  const n = { name: name, count: count };
  if (self) n.self = self;
  if (children && children.length) n.children = children;
  return n;
}

// ── mergeTrees: union by name, both sides accumulate ──
{
  // A: (all)=100 -> foo=100 -> bar=60(self 60), baz=40(self 40)
  const treeA = node("(all)", 100, 0, [
    node("foo", 100, 0, [node("bar", 60, 60), node("baz", 40, 40)]),
  ]);
  // B: (all)=200 -> foo=200 -> bar=20(self 20), qux=180(self 180)
  const treeB = node("(all)", 200, 0, [
    node("foo", 200, 0, [node("bar", 20, 20), node("qux", 180, 180)]),
  ]);
  const m = mergeTrees(treeA, treeB);

  assertEq(m.name, "(all)", "merged root keeps name");
  assertEq(m.a, 100, "root total A = capture A total");
  assertEq(m.b, 200, "root total B = capture B total");

  const foo = m.children.get("foo");
  assertEq(foo.a, 100, "foo total A");
  assertEq(foo.b, 200, "foo total B");
  assertEq(foo.children.size, 3, "foo has union of children (bar, baz, qux)");

  const bar = foo.children.get("bar");
  assertEq(bar.a, 60, "bar present on both sides: A total");
  assertEq(bar.b, 20, "bar present on both sides: B total");
  assertEq(bar.selfA, 60, "bar self A");
  assertEq(bar.selfB, 20, "bar self B");

  const baz = foo.children.get("baz");
  assertEq(baz.a, 40, "baz (A-only): A total");
  assertEq(baz.b, 0, "baz (A-only): B total is 0");

  const qux = foo.children.get("qux");
  assertEq(qux.a, 0, "qux (B-only): A total is 0");
  assertEq(qux.b, 180, "qux (B-only): B total");
}

// ── mergeTrees: one side missing entirely ──
{
  const treeB = node("(all)", 50, 0, [node("solo", 50, 50)]);
  const m = mergeTrees(null, treeB);
  assertEq(m.a, 0, "null A side contributes 0");
  assertEq(m.b, 50, "B side total carries through");
  assertEq(m.children.get("solo").b, 50, "B-only child present");
  assertEq(mergeTrees(null, null).name, "(all)", "both null -> default root name");
}

// ── mergeTrees: self defaults when wire omits `self`/`children` ──
{
  const m = mergeTrees(node("(all)", 10), node("(all)", 10));
  assertEq(m.selfA, 0, "missing self -> 0 (A)");
  assertEq(m.selfB, 0, "missing self -> 0 (B)");
  assertEq(m.children.size, 0, "missing children -> empty map");
}

// ── diffColor: parity is grey ──
assertEq(diffColor(50, 50, 100, 100), "rgb(207,207,207)", "equal fractions -> neutral grey");
assertEq(diffColor(0, 0, 100, 100), "rgb(207,207,207)", "absent on both sides -> grey");

// ── diffColor: direction (B heavier -> red channel dominates; A heavier -> blue) ──
{
  function rgb(str) {
    const m = /rgb\((\d+),(\d+),(\d+)\)/.exec(str);
    return { r: +m[1], g: +m[2], b: +m[3] };
  }
  const hotB = rgb(diffColor(10, 90, 100, 100)); // heavier in B
  assert(hotB.r > hotB.b, "heavier in B: red channel exceeds blue");
  const hotA = rgb(diffColor(90, 10, 100, 100)); // heavier in A
  assert(hotA.b > hotA.r, "heavier in A: blue channel exceeds red");
}

// ── diffColor: saturation clamps at +/-4 octaves ──
{
  // Past the +/-4-octave (16x fraction-ratio) clamp the color stops changing.
  // Large totals keep the epsilon floor negligible so the clamp, not epsilon,
  // is what's under test. 100x and 500x ratios are both well past the clamp.
  const clamped = diffColor(1000, 100000, 1000000, 1000000);
  const moreClamped = diffColor(1000, 500000, 1000000, 1000000);
  assertEq(moreClamped, clamped, "ratio beyond +/-4 octaves clamps to full saturation");
  assertEq(clamped, "rgb(255,40,16)", "full-saturation B is the fixed red endpoint");
}

// ── diffColor: one-sided frame is finite (epsilon floor), not Infinity/NaN ──
{
  const bOnly = diffColor(0, 100, 100, 100);
  assert(/^rgb\(\d+,\d+,\d+\)$/.test(bOnly), "B-only frame yields a valid rgb (no Infinity/NaN)");
  const aOnly = diffColor(100, 0, 100, 100);
  assert(/^rgb\(\d+,\d+,\d+\)$/.test(aOnly), "A-only frame yields a valid rgb (no Infinity/NaN)");
}

// ── diffColor: one-sided frame colors toward its side even when totals differ
//    wildly (the shared epsilon floor). Regression: a per-side floor (0.5/tA vs
//    0.5/tB) is not comparable across sides, so a frame unique to the much
//    larger capture used to color backwards (blue = "heavier in A" though it
//    never appears in A). yulnr's repro: A=1 sample, B=8846. ──
{
  function rgb(str) {
    const m = /rgb\((\d+),(\d+),(\d+)\)/.exec(str);
    return { r: +m[1], g: +m[2], b: +m[3] };
  }
  // A tiny B-only frame (1 of 8846) against a 1-sample A total must read as
  // heavier-in-B (red), not blue.
  const bOnlyTiny = rgb(diffColor(0, 1, 1, 8846));
  assert(bOnlyTiny.r > bOnlyTiny.b, "tiny B-only frame vs 1-sample A total -> red (heavier in B)");
  // A large B-only frame at the same lopsided totals is fully-saturated red.
  const bOnlyBig = rgb(diffColor(0, 8000, 1, 8846));
  assert(bOnlyBig.r > bOnlyBig.b, "large B-only frame vs 1-sample A total -> red (heavier in B)");
  // Mirror: a tiny A-only frame against a 1-sample B total reads as blue.
  const aOnlyTiny = rgb(diffColor(1, 0, 8846, 1));
  assert(aOnlyTiny.b > aOnlyTiny.r, "tiny A-only frame vs 1-sample B total -> blue (heavier in A)");
}

// ── diffColor: normalizes per-side, so equal FRACTIONS at unequal totals are grey ──
// Totals large enough that the per-side epsilon floor is negligible; both sides
// sit at 10% of their own capture despite B being 10x bigger.
assertEq(
  diffColor(10000, 100000, 100000, 1000000),
  "rgb(207,207,207)",
  "10% of A == 10% of B: self-normalized fractions equal -> grey despite 10x total",
);

// ── layoutSide / nodeAtPath: per-side width normalization and zoom paths ──
{
  // A: (all)=100 -> foo=100 -> bar=60, baz=40   (bar wider on A)
  // B: (all)=100 -> foo=100 -> bar=20, baz=80   (baz wider on B)
  const treeA = node("(all)", 100, 0, [
    node("foo", 100, 0, [node("bar", 60, 60), node("baz", 40, 40)]),
  ]);
  const treeB = node("(all)", 100, 0, [
    node("foo", 100, 0, [node("bar", 20, 20), node("baz", 80, 80)]),
  ]);
  const m = mergeTrees(treeA, treeB);

  // nodeAtPath
  assertEq(nodeAtPath(m, ["(all)"]), m, "nodeAtPath root returns root");
  assertEq(nodeAtPath(m, ["(all)", "foo", "bar"]).a, 60, "nodeAtPath descends to bar");
  assertEq(nodeAtPath(m, ["(all)", "nope"]), null, "nodeAtPath missing path -> null");

  // Layout side A from the root, 1000px wide.
  const la = layoutSide(m, ["(all)"], "a", 1000);
  const byName = {};
  for (const bx of la.boxes) byName[bx.name] = bx;
  assertEq(byName["(all)"].w, 1000, "side A root spans full width");
  assertEq(byName["foo"].w, 1000, "side A foo spans full width (100/100)");
  assertEq(byName["bar"].w, 600, "side A bar width = 60% of panel");
  assertEq(byName["baz"].w, 400, "side A baz width = 40% of panel");
  // Children ordered widest-first on this side: bar (60) left of baz (40).
  assert(byName["bar"].x < byName["baz"].x, "side A orders bar (wider) left of baz");
  assertEq(la.maxDepth, 2, "maxDepth counts (all)=0, foo=1, bar/baz=2");

  // Layout side B: baz now dominates, so baz is wider and leftmost.
  const lb = layoutSide(m, ["(all)"], "b", 1000);
  const bByName = {};
  for (const bx of lb.boxes) bByName[bx.name] = bx;
  assertEq(bByName["bar"].w, 200, "side B bar width = 20% (its own normalization)");
  assertEq(bByName["baz"].w, 800, "side B baz width = 80%");
  assert(bByName["baz"].x < bByName["bar"].x, "side B orders baz (wider) left of bar");

  // Each box carries both sides' counts (for shared color + tooltip).
  assertEq(byName["bar"].a, 60, "box carries A count");
  assertEq(byName["bar"].b, 20, "box carries B count");
  assertEq(byName["bar"].path.join(">"), "(all)>foo>bar", "box carries full path");

  // Zoomed layout: focus on foo, width normalizes to foo's own count.
  const zoomed = layoutSide(nodeAtPath(m, ["(all)", "foo"]), ["(all)", "foo"], "a", 1000);
  const zByName = {};
  for (const bx of zoomed.boxes) zByName[bx.name] = bx;
  assertEq(zByName["foo"].w, 1000, "zoom root foo spans full width");
  assertEq(zByName["bar"].w, 600, "zoomed bar still 60% of foo");

  // Sub-pixel boxes are dropped from the draw list.
  const tiny = layoutSide(m, ["(all)"], "a", 10); // bar=6px, baz=4px ok; deeper would vanish
  const narrow = layoutSide(
    mergeTrees(node("(all)", 10000, 0, [node("big", 9999, 9999), node("sliver", 1, 1)]), null),
    ["(all)"], "a", 100, 0.4,
  );
  const names = narrow.boxes.map((b) => b.name);
  assert(names.indexOf("sliver") === -1, "sub-pixel box (sliver) is skipped");
  assert(names.indexOf("big") !== -1, "wide box is kept");
}

// ── fullScopeQuery: keeps the complete scope incl. bucket/prefix, drops creds ──
{
  const src = new URLSearchParams();
  src.set("api", "1");
  src.set("bucket", "my-bucket");
  src.set("aws_region", "us-west-2");
  src.set("prefix", "traces/svc");
  src.set("service", "svc");
  src.append("host", "h1");
  src.append("host", "h2");
  src.set("thread_class", "worker");
  src.set("source", "cpu");
  src.set("start_ns", "1782155999000000000");
  src.set("end_ns", "1782159599000000000");
  src.set("max_files", "256");
  // Non-scope junk that must NOT be carried (e.g. a credential header value
  // someone wrongly stuffed in the query, or a transient zoom param).
  src.set("worker-zoom", "foo\tbar");
  src.set("x-dial9-aws-access-key-id", "AKIASECRET");

  const out = fullScopeQuery(src);
  assertEq(out.get("bucket"), "my-bucket", "bucket survives (fixes lossy address bar)");
  assertEq(out.get("aws_region"), "us-west-2", "region survives (cross-region bucket link)");
  assertEq(out.get("prefix"), "traces/svc", "prefix survives");
  assertEq(out.get("max_files"), "256", "max_files survives");
  assertEq(out.getAll("host").join(","), "h1,h2", "repeatable host set survives in order");
  assertEq(out.get("worker-zoom"), null, "transient zoom param dropped");
  assertEq(out.get("x-dial9-aws-access-key-id"), null, "credential-shaped param never carried");
}

// ── fullScopeQuery: empty/absent values are omitted (not emitted as "") ──
{
  const src = new URLSearchParams("api=1&service=&bucket=b");
  const out = fullScopeQuery(src);
  assertEq(out.get("service"), null, "empty-string value is omitted");
  assertEq(out.get("bucket"), "b", "present value kept");
  assertEq(out.has("host"), false, "no host -> none emitted");
}

// ── base64url codec round-trip ──
{
  const samples = [
    "api=1&bucket=b&prefix=traces%2Fsvc&host=h1&host=h2",
    "",
    "a=1&b=2&c=3",
  ];
  for (const s of samples) {
    assertEq(b64urlDecode(b64urlEncode(s)), s, `b64url round-trip: ${JSON.stringify(s)}`);
  }
  // base64url must not contain +, /, or = padding.
  const enc = b64urlEncode("api=1&bucket=my-bucket&prefix=a/b/c?d=e");
  assert(!/[+/=]/.test(enc), "base64url output has no +, /, or = chars");
}

// ── encodeScope / decodeScope: scope params survive the round-trip ──
{
  const scope = fullScopeQuery(
    new URLSearchParams("api=1&bucket=b&prefix=traces/svc&host=h1&host=h2&source=cpu"),
  );
  const back = decodeScope(encodeScope(scope));
  assertEq(back.get("bucket"), "b", "decoded bucket");
  assertEq(back.get("prefix"), "traces/svc", "decoded prefix");
  assertEq(back.getAll("host").join(","), "h1,h2", "decoded repeatable hosts");
  assertEq(back.get("source"), "cpu", "decoded source");
}

// ── diffSearch / parseDiff: full link round-trip ──
{
  const scopeA = fullScopeQuery(new URLSearchParams("api=1&bucket=ba&prefix=pa&service=svc&host=h1"));
  const scopeB = fullScopeQuery(new URLSearchParams("api=1&bucket=bb&prefix=pb&service=svc&host=h2"));
  const search = diffSearch(scopeA, scopeB);

  assert(search.indexOf("diff=1") === 0, "diff link starts with diff=1");
  const parsed = parseDiff(search);
  assert(parsed != null, "parseDiff recognizes a diff link");
  assertEq(parsed.a.get("bucket"), "ba", "side A bucket round-trips");
  assertEq(parsed.b.get("bucket"), "bb", "side B bucket round-trips (different BYOC bucket)");
  assertEq(parsed.a.get("host"), "h1", "side A host");
  assertEq(parsed.b.get("host"), "h2", "side B host");
  // The encoded blobs must not leak the raw scope (so `bucket=` doesn't appear
  // literally in the diff query — it's base64url'd inside a= / b=).
  assert(search.indexOf("bucket=") === -1, "raw scope keys are encoded, not literal in the link");
}

// ── diffSearch carries INDEPENDENT per-side time windows ──
// The landing-page "Add to diff" flow (and the tokio-stats diff that consumes
// the same link) capture A and B as fully independent scopes: not just
// different buckets/hosts, but different time windows. Both sides' start_ns /
// end_ns must survive the codec independently.
{
  const scopeA = fullScopeQuery(new URLSearchParams(
    "api=1&bucket=b&prefix=p&service=svc&host=h1&start_ns=1000&end_ns=2000"));
  const scopeB = fullScopeQuery(new URLSearchParams(
    "api=1&bucket=b&prefix=p&service=svc&host=h1&start_ns=8000&end_ns=9000"));
  const parsed = parseDiff(diffSearch(scopeA, scopeB));
  assertEq(parsed.a.get("start_ns"), "1000", "side A start_ns round-trips");
  assertEq(parsed.a.get("end_ns"), "2000", "side A end_ns round-trips");
  assertEq(parsed.b.get("start_ns"), "8000", "side B start_ns (independent window) round-trips");
  assertEq(parsed.b.get("end_ns"), "9000", "side B end_ns (independent window) round-trips");
}

// ── diffSearch carries INDEPENDENT per-side poll-duration bands ──
// The marquee "why are the slow polls slow" diff: same service/host, A = fast
// polls, B = slow polls. Both sides' min_poll_ns / max_poll_ns must survive the
// codec independently, exactly like the per-side time windows above.
{
  const scopeA = fullScopeQuery(new URLSearchParams(
    "api=1&bucket=b&prefix=p&service=svc&host=h1&max_poll_ns=1000000"));
  const scopeB = fullScopeQuery(new URLSearchParams(
    "api=1&bucket=b&prefix=p&service=svc&host=h1&min_poll_ns=10000000"));
  const parsed = parseDiff(diffSearch(scopeA, scopeB));
  assertEq(parsed.a.get("max_poll_ns"), "1000000", "side A max_poll_ns (fast polls ≤1ms) round-trips");
  assertEq(parsed.a.get("min_poll_ns"), null, "side A has no lower bound");
  assertEq(parsed.b.get("min_poll_ns"), "10000000", "side B min_poll_ns (slow polls ≥10ms) round-trips");
  assertEq(parsed.b.get("max_poll_ns"), null, "side B has no upper bound");
}

// ── pollBandLabel: human band summary for the scope header / diff legend ──
assertEq(pollBandLabel(null, null), "", "no band -> empty label");
assertEq(pollBandLabel("", ""), "", "empty-string bounds -> empty label");
assertEq(pollBandLabel("10000000", null), "poll ≥ 10ms", "lower bound only");
assertEq(pollBandLabel(null, "1000000"), "poll ≤ 1ms", "upper bound only");
assertEq(pollBandLabel("1000000", "10000000"), "poll 1–10ms", "both bounds -> range");
assertEq(pollBandLabel("500000", null), "poll ≥ 0.5ms", "sub-ms bound formats with decimal");
assertEq(pollBandLabel(1000000, 10000000), "poll 1–10ms", "accepts numbers as well as strings");

// ── parseDiff: rejects non-diff and malformed links ──
assertEq(parseDiff("api=1&bucket=b"), null, "api-mode (non-diff) link -> null");
assertEq(parseDiff("diff=1&a=abc"), null, "diff link missing b -> null");
assertEq(parseDiff("diff=1"), null, "diff link missing both a and b -> null");
assert(parseDiff(new URLSearchParams("diff=1&a=" + encodeScope("bucket=x") + "&b=" + encodeScope("bucket=y"))) != null,
  "parseDiff accepts a URLSearchParams as well as a string");

// ── chooseTarget: single-scope vs captured A/B diff routing (issue #626) ──
// The landing page's top "Flamegraph"/"Tokio Stats" buttons route through the
// same seam as the diff tray's launch buttons, so once a full diff is captured
// the top button opens the diff instead of a single scope.
{
  const diffA = fullScopeQuery(new URLSearchParams("bucket=ba&service=svc&host=h1"));
  const diffB = fullScopeQuery(new URLSearchParams("bucket=bb&service=svc&host=h2"));

  // hasDiff true -> diff link, correct page per kind, per-side api=1 for flamegraph.
  const fg = chooseTarget("flamegraph", { hasDiff: true, diffA, diffB });
  assertEq(fg.page, "flamegraph.html", "diff mode flamegraph -> flamegraph.html");
  assert(fg.search.indexOf("diff=1&a=") === 0, "diff mode flamegraph search starts with diff=1&a=..");
  assert(fg.search.indexOf("&b=") !== -1, "diff mode flamegraph search carries b=..");
  const fgParsed = parseDiff(fg.search);
  assertEq(fgParsed.a.get("bucket"), "ba", "diff mode flamegraph: side A scope round-trips");
  assertEq(fgParsed.a.get("api"), "1", "diff mode flamegraph: per-side api=1 set on A");
  assertEq(fgParsed.b.get("api"), "1", "diff mode flamegraph: per-side api=1 set on B");

  const tk = chooseTarget("tokio", { hasDiff: true, diffA, diffB });
  assertEq(tk.page, "tokio_stats.html", "diff mode tokio -> tokio_stats.html");
  assert(tk.search.indexOf("diff=1&a=") === 0, "diff mode tokio search starts with diff=1&a=..");
  const tkParsed = parseDiff(tk.search);
  assertEq(tkParsed.a.get("api"), null, "diff mode tokio: no api flag (tokio-stats does not use it)");

  // hasDiff false -> the caller's pre-built single-scope query, unchanged.
  const single = chooseTarget("flamegraph", { hasDiff: false, singleQuery: "api=1&bucket=b&service=svc" });
  assertEq(single.page, "flamegraph.html", "single mode -> flamegraph.html");
  assertEq(single.search, "api=1&bucket=b&service=svc", "single mode passes the single-scope query through unchanged");
  assert(parseDiff(single.search) == null, "single mode search is not a diff link");
  assertEq(
    chooseTarget("tokio", { hasDiff: false, singleQuery: "bucket=b&service=svc" }).page,
    "tokio_stats.html",
    "single mode tokio -> tokio_stats.html",
  );
}

// ── flamegraph_diff_view: apiUrlFor / scopeLabel (DOM-free helpers) ──
{
  const V = require("./flamegraph_diff_view.js");
  const scope = new URLSearchParams(
    "api=1&bucket=b&aws_region=us-west-2&prefix=traces/svc&service=svc&host=h1&host=h2&source=cpu&start_ns=100&max_files=64",
  );
  const u = V.apiUrlFor({ scope, origin: "https://viewer.example.com" });
  assertEq(u.pathname, "/api/flamegraph", "apiUrlFor targets /api/flamegraph");
  assertEq(u.searchParams.get("bucket"), "b", "apiUrlFor forwards bucket");
  assertEq(u.searchParams.get("aws_region"), "us-west-2",
    "apiUrlFor forwards region (side B often has a cross-region bucket)");
  assertEq(u.searchParams.get("max_files"), "64", "apiUrlFor forwards max_files");
  assertEq(u.searchParams.getAll("host").join(","), "h1,h2", "apiUrlFor forwards repeatable hosts");
  assertEq(u.searchParams.get("api"), null, "apiUrlFor does NOT forward the client-only api flag");
  assertEq(u.searchParams.get("refine"), null,
    "no refine param — the endpoint is an SSE stream and the server owns refinement");

  // The diff view drives the per-side sampling cap itself (small initial fold,
  // raised by "Load more"): an explicit maxFiles arg overrides the scope's own
  // max_files, and a scope with none gets exactly the override.
  const uOverride = V.apiUrlFor({ scope, origin: "https://viewer.example.com", maxFiles: 8 });
  assertEq(uOverride.searchParams.get("max_files"), "8", "apiUrlFor maxFiles arg overrides the scope's max_files");
  const scopeNoMax = new URLSearchParams("bucket=b&service=svc");
  assertEq(V.apiUrlFor({ scope: scopeNoMax, origin: "https://viewer.example.com" }).searchParams.get("max_files"), null,
    "no maxFiles arg and no scope max_files -> none set (server default)");
  assertEq(V.apiUrlFor({ scope: scopeNoMax, origin: "https://viewer.example.com", maxFiles: 8 }).searchParams.get("max_files"), "8",
    "maxFiles arg sets the cap even when the scope carries none");

  assertEq(V.scopeLabel(new URLSearchParams("service=svc&host=h1"), "A"), "svc @ h1", "label: single host");
  assertEq(V.scopeLabel(new URLSearchParams("service=svc"), "A"), "svc", "label: no host");
  assertEq(V.scopeLabel(new URLSearchParams("service=svc&host=a&host=b&host=c"), "A"), "svc @ 3 hosts", "label: host count");
  assertEq(V.scopeLabel(new URLSearchParams("host=h1"), "A"), "A @ h1", "label: no service falls back");

  // isSearchFocusKey: "/" focuses the highlight box only when it isn't already
  // focused (so a literal "/" can be typed into the regex); Ctrl/Cmd+F always
  // focuses; other keys never do.
  assertEq(V.isSearchFocusKey({ key: "/" }, false), true, "'/' focuses when not already in the search box");
  assertEq(V.isSearchFocusKey({ key: "/" }, true), false, "'/' does not steal focus while typing in the search box");
  assertEq(V.isSearchFocusKey({ key: "f", ctrlKey: true }, false), true, "Ctrl+F focuses the search box");
  assertEq(V.isSearchFocusKey({ key: "f", metaKey: true }, false), true, "Cmd+F focuses the search box");
  assertEq(V.isSearchFocusKey({ key: "a" }, false), false, "other keys do not focus the search box");
}

// ── addDiffCapture / swapDiffCapture / removeDiffSide: tray state machine ──
// Backs the in-page "Add to diff" tray on the flamegraph aggregate toolbar
// (issue #646): capture fills A first, then B, then replaces B; A always fills
// before B (no B-without-A hole).
{
  const sa = new URLSearchParams("api=1&bucket=b&service=svc&host=host-a");
  const sb = new URLSearchParams("api=1&bucket=b&service=svc&host=host-b");
  const sc = new URLSearchParams("api=1&bucket=b&service=svc&host=host-c");

  let st = { a: null, b: null };
  st = addDiffCapture(st, sa);
  assertEq(st.a, sa, "first add fills A");
  assertEq(st.b, null, "first add leaves B empty");
  st = addDiffCapture(st, sb);
  assertEq(st.a, sa, "second add keeps A");
  assertEq(st.b, sb, "second add fills B");
  st = addDiffCapture(st, sc);
  assertEq(st.a, sa, "third add keeps A");
  assertEq(st.b, sc, "third add replaces B (most recent)");

  // Swap flips A/B only when both are set.
  const sw = swapDiffCapture(st);
  assertEq(sw.a, sc, "swap makes old B the new A");
  assertEq(sw.b, sa, "swap makes old A the new B");
  const swLone = swapDiffCapture({ a: sa, b: null });
  assertEq(swLone.a, sa, "swap with a lone A is a no-op (keeps A)");
  assertEq(swLone.b, null, "swap with a lone A leaves B empty");

  // Removing A promotes B so there is never a B-without-A hole.
  const rmA = removeDiffSide({ a: sa, b: sb }, "a");
  assertEq(rmA.a, sb, "removing A promotes B into A");
  assertEq(rmA.b, null, "removing A leaves B empty");
  const rmB = removeDiffSide({ a: sa, b: sb }, "b");
  assertEq(rmB.a, sa, "removing B keeps A");
  assertEq(rmB.b, null, "removing B clears B");

  // Inputs are not mutated in place (transitions return fresh state objects).
  const orig = { a: sa, b: null };
  addDiffCapture(orig, sb);
  assertEq(orig.b, null, "addDiffCapture does not mutate the input state");
}

// ── Capture → diff round-trip for host-vs-host (issue #646) ──
// The core of the "Add to diff" flow: capture the current view narrowed to one
// host as side A, re-narrow to a second host and capture as side B, then open
// the two-sided diff. Both host selections must survive the codec independently,
// as must the shared scope (api/bucket/service).
{
  const scopeA = fullScopeQuery(new URLSearchParams("api=1&bucket=b&prefix=p&service=svc&host=host-a"));
  const scopeB = fullScopeQuery(new URLSearchParams("api=1&bucket=b&prefix=p&service=svc&host=host-b"));

  let st = { a: null, b: null };
  st = addDiffCapture(st, scopeA);
  st = addDiffCapture(st, scopeB);
  const parsed = parseDiff(diffSearch(st.a, st.b));
  assert(parsed != null, "capture -> diffSearch -> parseDiff round-trips");
  assertEq(parsed.a.get("host"), "host-a", "side A narrowed to host-a survives");
  assertEq(parsed.b.get("host"), "host-b", "side B narrowed to host-b survives");
  assertEq(parsed.a.get("api"), "1", "side A carries api=1 (aggregate path)");
  assertEq(parsed.b.get("api"), "1", "side B carries api=1 (aggregate path)");
  assertEq(parsed.a.get("bucket"), "b", "shared bucket survives on A");
  assertEq(parsed.b.get("service"), "svc", "shared service survives on B");
}

// ── shiftScopeTime: earlier-window presets (issue #624) ──
// start_ns/end_ns are ~1.78e18, far above Number.MAX_SAFE_INTEGER, so the shift
// must use BigInt or the values corrupt. Assert exact string equality against
// BigInt-computed expectations, that the window LENGTH is preserved, and that
// non-time params are untouched.
{
  const scope = fullScopeQuery(new URLSearchParams(
    "api=1&bucket=b&prefix=p&service=svc&host=h1&start_ns=1782155999000000000&end_ns=1782159599000000000"));

  const shifted = shiftScopeTime(scope, DIFF_SHIFT_24H);
  // 1782155999000000000 - 86400000000000 = 1782069599000000000
  assertEq(shifted.get("start_ns"), "1782069599000000000", "-24h shifts start_ns back by exactly the delta");
  assertEq(shifted.get("end_ns"), "1782073199000000000", "-24h shifts end_ns back by exactly the delta");
  // Window length preserved.
  const origLen = BigInt(scope.get("end_ns")) - BigInt(scope.get("start_ns"));
  const newLen = BigInt(shifted.get("end_ns")) - BigInt(shifted.get("start_ns"));
  assert(origLen === newLen, "-24h preserves the window length");
  // Non-time params untouched.
  assertEq(shifted.get("bucket"), "b", "shiftScopeTime leaves bucket untouched");
  assertEq(shifted.get("service"), "svc", "shiftScopeTime leaves service untouched");
  assertEq(shifted.get("host"), "h1", "shiftScopeTime leaves host untouched");

  // -1h and -7d line up with their BigInt deltas too.
  const s1h = shiftScopeTime(scope, DIFF_SHIFT_1H);
  assertEq(s1h.get("start_ns"), (BigInt(scope.get("start_ns")) - DIFF_SHIFT_1H).toString(), "-1h start_ns matches BigInt delta");
  const s7d = shiftScopeTime(scope, DIFF_SHIFT_7D);
  assertEq(s7d.get("start_ns"), (BigInt(scope.get("start_ns")) - DIFF_SHIFT_7D).toString(), "-7d start_ns matches BigInt delta");

  // No window -> returned unchanged (no start_ns/end_ns to shift).
  const noWindow = fullScopeQuery(new URLSearchParams("api=1&bucket=b&service=svc&host=h1"));
  const nwShifted = shiftScopeTime(noWindow, DIFF_SHIFT_24H);
  assertEq(nwShifted.get("start_ns"), null, "no start_ns -> none introduced");
  assertEq(nwShifted.get("end_ns"), null, "no end_ns -> none introduced");
  assertEq(nwShifted.get("bucket"), "b", "windowless scope carries other params through");

  // Input is not mutated.
  assertEq(scope.get("start_ns"), "1782155999000000000", "shiftScopeTime does not mutate the input scope");
}

// ── scopeWithHost: same-time-different-host preset (issue #624) ──
{
  const scope = fullScopeQuery(new URLSearchParams(
    "api=1&bucket=b&prefix=p&service=svc&host=h1&host=h2&start_ns=1000&end_ns=2000"));

  const swapped = scopeWithHost(scope, "h3");
  assertEq(swapped.getAll("host").join(","), "h3", "scopeWithHost replaces all hosts with exactly the chosen one");
  assertEq(swapped.get("bucket"), "b", "scopeWithHost preserves bucket");
  assertEq(swapped.get("prefix"), "p", "scopeWithHost preserves prefix");
  assertEq(swapped.get("service"), "svc", "scopeWithHost preserves service");
  assertEq(swapped.get("start_ns"), "1000", "scopeWithHost preserves start_ns");
  assertEq(swapped.get("end_ns"), "2000", "scopeWithHost preserves end_ns");

  // Input is not mutated.
  assertEq(scope.getAll("host").join(","), "h1,h2", "scopeWithHost does not mutate the input scope");

  // End-to-end through the existing codec: preset 1 builds a diff link whose
  // side B is the chosen host at side A's window.
  const parsed = parseDiff(diffSearch(scope, scopeWithHost(scope, "h3")));
  assert(parsed != null, "preset 1 -> diffSearch -> parseDiff round-trips");
  assertEq(parsed.b.get("host"), "h3", "preset 1: side B narrowed to the chosen host");
  assertEq(parsed.b.get("start_ns"), scope.get("start_ns"), "preset 1: side B keeps side A's window (same time)");
  assertEq(parsed.a.getAll("host").join(","), "h1,h2", "preset 1: side A scope unchanged");
}

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
