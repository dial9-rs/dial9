// flamegraph_export.js - Export a rendered flamegraph tree to portable formats.
//
// The viewer renders flamegraphs onto a <canvas>, which cannot be saved or
// shared. This module turns the same tree the canvas is painted from (the
// `{name, fullName, location, count, self, children}` shape produced by
// trace_analysis.js `buildFlamegraphTree`) into two portable artifacts:
//
//   - Interactive SVG: a standalone, self-contained flame graph that behaves
//     like Brendan Gregg's flamegraph.pl / the flamegraph-rs (inferno) output:
//     hover a frame for details, click to zoom, Ctrl-F to regex-search with a
//     "Matched: N%" readout, Reset Zoom, Ctrl-I to toggle case sensitivity, and
//     URL state (?x=&y=&s=). The embedded SVG structure and the embedded
//     <script> are ported, near-verbatim, from flamegraph.pl so the behavior is
//     identical to the canonical tool. No external assets, no network.
//   - Folded stacks: the universal `frame1;frame2;frame3 <count>` text format
//     consumed by inferno, flamegraph.pl, and speedscope. Carries the FULL tree
//     (no visual filtering) so no data is lost for downstream tools.
//
// Pure functions only — no DOM, no globals — so they run under node for tests.
//
// The embedded interactive script (embeddedScript function) and the surrounding
// SVG structure are derived from flamegraph.pl by Brendan Gregg, licensed under
// the CDDL 1.0:
//   Copyright 2016 Netflix, Inc.
//   Copyright 2011 Joyent, Inc. All rights reserved.
//   Copyright 2011 Brendan Gregg. All rights reserved.
//   https://github.com/brendangregg/FlameGraph
//   License: CDDL-1.0 (see THIRD_PARTY_LICENSES)

(function (exports) {
  "use strict";

  // ── geometry, ported from flamegraph.pl tunables ──
  const FONT_SIZE = 12;
  const FONT_WIDTH = 0.59; // avg glyph width relative to font size
  const FRAME_HEIGHT = 16;
  const FRAME_PAD = 1; // vertical gap between frames
  const XPAD = 10; // left/right pad
  const YPAD1 = FONT_SIZE * 3; // top pad (title)
  const YPAD2 = FONT_SIZE * 2 + 10; // bottom pad (labels)
  const DEFAULT_WIDTH = 1200;
  const MINWIDTH_PX = 0.1; // omit frames narrower than this many pixels
  const FONT_TYPE = "Verdana";
  const SEARCH_COLOR = "rgb(230,0,230)";
  const BG1 = "#eeeeee";
  const BG2 = "#eeeeb0";

  // The frame color is the SAME hash-to-warm-color mapping the on-screen canvas
  // uses (hue range 10–50, red/orange — flamegraph.pl's "hot" palette family),
  // sourced from trace_analysis.js so the export and the canvas can never drift.
  function getAnalysis() {
    if (typeof require !== "undefined") return require("./trace_analysis.js");
    if (typeof TraceAnalysis !== "undefined") return TraceAnalysis;
    throw new Error(
      "TraceAnalysis not found. Load trace_analysis.js before flamegraph_export.js"
    );
  }
  const flamegraphColor = getAnalysis().flamegraphColor;

  function escapeXml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&apos;");
  }

  // 1234567 -> "1,234,567"
  function withCommas(n) {
    const s = String(n);
    return s.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  }

  // ── Folded stacks ──────────────────────────────────────────────────────
  // Walk a tree into the universal folded-stacks format. Each frame contributes
  // its own self-weight under the full path from root to that frame. The
  // synthetic "(all)" root is not emitted as a path component. Counts are
  // rounded to integers because the folded format and its consumers (inferno,
  // flamegraph.pl) expect integer sample counts; the SVG keeps exact weights.
  function treeToFolded(root) {
    if (!root) return "";
    const lines = [];
    function walk(node, stack) {
      const label = node === root ? null : (node.fullName || node.name);
      const path = label == null ? stack : stack.concat(label);
      const self = Math.round(node.self || 0);
      if (self > 0 && path.length > 0) {
        lines.push(path.join(";") + " " + self);
      }
      const kids = [...node.children.values()].sort((a, b) => b.count - a.count);
      for (const child of kids) walk(child, path);
    }
    walk(root, []);
    return lines.join("\n") + (lines.length ? "\n" : "");
  }

  // ── Interactive SVG ────────────────────────────────────────────────────

  // Merge the (worker / off-worker) panels into ONE single-rooted tree, which
  // is what a normal flame graph is. flamegraph.pl's embedded JS assumes a
  // single full-width root frame (its search percentage relies on it), so:
  //   - one panel with data  → use that panel's tree directly as the root.
  //   - several panels        → synthesize a root whose children are the panels,
  //     each wrapped in a labeled frame, preserving the worker/off-worker split
  //     while keeping a single zoomable, searchable graph.
  // Returns null when there is nothing to draw. Read-only: never mutates inputs.
  function buildExportRoot(panels) {
    const usable = (panels || []).filter((p) => p && p.tree && p.tree.count > 0);
    if (usable.length === 0) return null;
    if (usable.length === 1) return usable[0].tree;
    let total = 0;
    const children = new Map();
    for (const p of usable) {
      total += p.tree.count;
      children.set("[" + p.label + "]", {
        name: "[" + p.label + "]",
        fullName: "[" + p.label + "]",
        location: null,
        count: p.tree.count,
        self: 0,
        children: p.tree.children,
      });
    }
    return { name: "", fullName: "", location: null, count: total, self: 0, children };
  }

  // Lay a tree out into flat positioned frames using flamegraph.pl's geometry:
  // x is cumulative count-space, children packed left-to-right within their
  // parent's span, sorted by descending count (matching the on-screen view).
  // Frames (and their subtrees) narrower than `minTime` count-units are pruned,
  // exactly like flamegraph.pl's narrow-block removal.
  function layoutTree(root, minTime) {
    const frames = [];
    let maxDepth = 0;
    function walk(node, depth, sTime) {
      const eTime = sTime + node.count;
      if (eTime - sTime < minTime && depth > 0) return; // keep the root always
      if (depth > maxDepth) maxDepth = depth;
      frames.push({ node, depth, sTime, eTime });
      let cx = sTime;
      const kids = [...node.children.values()].sort((a, b) => b.count - a.count);
      for (const child of kids) {
        walk(child, depth + 1, cx);
        cx += child.count;
      }
    }
    walk(root, 0, 0);
    return { frames, maxDepth };
  }

  // The embedded interactive script, ported from flamegraph.pl's CDATA block
  // (https://github.com/brendangregg/FlameGraph). Perl tunables are substituted
  // with the literals above (xpad=10, fontsize=12, fontwidth=0.59, inverted=0).
  // Behavior is intentionally identical: click=zoom, mouseover=details,
  // Ctrl-F=search, Ctrl-I=ignorecase, Reset Zoom, and URL state restore.
  //
  // Derived from flamegraph.pl, © Netflix/Joyent/Brendan Gregg, licensed under
  // CDDL-1.0. See the file header and THIRD_PARTY_LICENSES for the full notice.
  function embeddedScript() {
    return [
      '"use strict";',
      "var details, searchbtn, unzoombtn, matchedtxt, svg, searching, currentSearchTerm, ignorecase, ignorecaseBtn;",
      "function init(evt) {",
      '  details = document.getElementById("details").firstChild;',
      '  searchbtn = document.getElementById("search");',
      '  ignorecaseBtn = document.getElementById("ignorecase");',
      '  unzoombtn = document.getElementById("unzoom");',
      '  matchedtxt = document.getElementById("matched");',
      '  svg = document.getElementsByTagName("svg")[0];',
      "  searching = 0;",
      "  currentSearchTerm = null;",
      "  var params = get_params();",
      "  if (params.x && params.y)",
      "    zoom(find_group(document.querySelector('[x=\"' + params.x + '\"][y=\"' + params.y + '\"]')));",
      "  if (params.s) search(params.s);",
      "}",
      'window.addEventListener("click", function(e) {',
      "  var target = find_group(e.target);",
      "  if (target) {",
      '    if (target.nodeName == "a") { if (e.ctrlKey === false) return; e.preventDefault(); }',
      '    if (target.classList.contains("parent")) unzoom(true);',
      "    zoom(target);",
      "    if (!document.querySelector('.parent')) {",
      "      var params = get_params();",
      "      if (params.x) delete params.x;",
      "      if (params.y) delete params.y;",
      "      history.replaceState(null, null, parse_params(params));",
      '      unzoombtn.classList.add("hide");',
      "      return;",
      "    }",
      '    var el = target.querySelector("rect");',
      "    if (el && el.attributes && el.attributes.y && el.attributes._orig_x) {",
      "      var params = get_params();",
      "      params.x = el.attributes._orig_x.value;",
      "      params.y = el.attributes.y.value;",
      "      history.replaceState(null, null, parse_params(params));",
      "    }",
      "  }",
      '  else if (e.target.id == "unzoom") clearzoom();',
      '  else if (e.target.id == "search") search_prompt();',
      '  else if (e.target.id == "ignorecase") toggle_ignorecase();',
      "}, false)",
      'window.addEventListener("mouseover", function(e) {',
      "  var target = find_group(e.target);",
      '  if (target) details.nodeValue = "Function: " + g_to_text(target);',
      "}, false)",
      'window.addEventListener("mouseout", function(e) {',
      "  var target = find_group(e.target);",
      "  if (target) details.nodeValue = ' ';",
      "}, false)",
      'window.addEventListener("keydown",function (e) {',
      "  if (e.keyCode === 114 || (e.ctrlKey && e.keyCode === 70)) { e.preventDefault(); search_prompt(); }",
      "  else if (e.ctrlKey && e.keyCode === 73) { e.preventDefault(); toggle_ignorecase(); }",
      "}, false)",
      "function get_params() {",
      "  var params = {};",
      "  var paramsarr = window.location.search.substr(1).split('&');",
      "  for (var i = 0; i < paramsarr.length; ++i) {",
      '    var tmp = paramsarr[i].split("=");',
      "    if (!tmp[0] || !tmp[1]) continue;",
      "    params[tmp[0]]  = decodeURIComponent(tmp[1]);",
      "  }",
      "  return params;",
      "}",
      "function parse_params(params) {",
      '  var uri = "?";',
      "  for (var key in params) { uri += key + '=' + encodeURIComponent(params[key]) + '&'; }",
      '  if (uri.slice(-1) == "&") uri = uri.substring(0, uri.length - 1);',
      "  if (uri == '?') uri = window.location.href.split('?')[0];",
      "  return uri;",
      "}",
      "function find_child(node, selector) {",
      "  var children = node.querySelectorAll(selector);",
      "  if (children.length) return children[0];",
      "}",
      "function find_group(node) {",
      "  var parent = node.parentElement;",
      "  if (!parent) return;",
      '  if (parent.id == "frames") return node;',
      "  return find_group(parent);",
      "}",
      "function orig_save(e, attr, val) {",
      '  if (e.attributes["_orig_" + attr] != undefined) return;',
      "  if (e.attributes[attr] == undefined) return;",
      "  if (val == undefined) val = e.attributes[attr].value;",
      '  e.setAttribute("_orig_" + attr, val);',
      "}",
      "function orig_load(e, attr) {",
      '  if (e.attributes["_orig_"+attr] == undefined) return;',
      '  e.attributes[attr].value = e.attributes["_orig_" + attr].value;',
      '  e.removeAttribute("_orig_"+attr);',
      "}",
      "function g_to_text(e) {",
      '  var text = find_child(e, "title").firstChild.nodeValue;',
      "  return (text)",
      "}",
      "function g_to_func(e) {",
      "  var func = g_to_text(e);",
      "  return (func);",
      "}",
      "function update_text(e) {",
      '  var r = find_child(e, "rect");',
      '  var t = find_child(e, "text");',
      "  var w = parseFloat(r.attributes.width.value) -3;",
      '  var txt = find_child(e, "title").textContent.replace(/\\([^(]*\\)$/,"");',
      "  t.attributes.x.value = parseFloat(r.attributes.x.value) + 3;",
      "  if (w < 2 * " + FONT_SIZE + " * " + FONT_WIDTH + ") { t.textContent = \"\"; return; }",
      "  t.textContent = txt;",
      "  var sl = t.getSubStringLength(0, txt.length);",
      "  if (/^ *$/.test(txt) || sl < w) return;",
      "  var start = Math.floor((w/sl) * txt.length);",
      "  for (var x = start; x > 0; x = x-2) {",
      "    if (t.getSubStringLength(0, x + 2) <= w) { t.textContent = txt.substring(0, x) + '..'; return; }",
      "  }",
      '  t.textContent = "";',
      "}",
      "function zoom_reset(e) {",
      "  if (e.attributes != undefined) { orig_load(e, 'x'); orig_load(e, 'width'); }",
      "  if (e.childNodes == undefined) return;",
      "  for (var i = 0, c = e.childNodes; i < c.length; i++) { zoom_reset(c[i]); }",
      "}",
      "function zoom_child(e, x, ratio) {",
      "  if (e.attributes != undefined) {",
      "    if (e.attributes.x != undefined) {",
      "      orig_save(e, 'x');",
      "      e.attributes.x.value = (parseFloat(e.attributes.x.value) - x - " + XPAD + ") * ratio + " + XPAD + ";",
      "      if (e.tagName == 'text') e.attributes.x.value = find_child(e.parentNode, 'rect[x]').attributes.x.value + 3;",
      "    }",
      "    if (e.attributes.width != undefined) {",
      "      orig_save(e, 'width');",
      "      e.attributes.width.value = parseFloat(e.attributes.width.value) * ratio;",
      "    }",
      "  }",
      "  if (e.childNodes == undefined) return;",
      "  for (var i = 0, c = e.childNodes; i < c.length; i++) { zoom_child(c[i], x - " + XPAD + ", ratio); }",
      "}",
      "function zoom_parent(e) {",
      "  if (e.attributes) {",
      "    if (e.attributes.x != undefined) { orig_save(e, 'x'); e.attributes.x.value = " + XPAD + "; }",
      "    if (e.attributes.width != undefined) { orig_save(e, 'width'); e.attributes.width.value = parseInt(svg.width.baseVal.value) - (" + XPAD + " * 2); }",
      "  }",
      "  if (e.childNodes == undefined) return;",
      "  for (var i = 0, c = e.childNodes; i < c.length; i++) { zoom_parent(c[i]); }",
      "}",
      "function zoom(node) {",
      "  var attr = find_child(node, 'rect').attributes;",
      "  var width = parseFloat(attr.width.value);",
      "  var xmin = parseFloat(attr.x.value);",
      "  var xmax = parseFloat(xmin + width);",
      "  var ymin = parseFloat(attr.y.value);",
      "  var ratio = (svg.width.baseVal.value - 2 * " + XPAD + ") / width;",
      "  var fudge = 0.0001;",
      "  unzoombtn.classList.remove('hide');",
      "  var el = document.getElementById('frames').children;",
      "  for (var i = 0; i < el.length; i++) {",
      "    var e = el[i];",
      "    var a = find_child(e, 'rect').attributes;",
      "    var ex = parseFloat(a.x.value);",
      "    var ew = parseFloat(a.width.value);",
      "    var upstack;",
      "    upstack = parseFloat(a.y.value) > ymin;",
      "    if (upstack) {",
      "      if (ex <= xmin && (ex+ew+fudge) >= xmax) { e.classList.add('parent'); zoom_parent(e); update_text(e); }",
      "      else e.classList.add('hide');",
      "    } else {",
      "      if (ex < xmin || ex + fudge >= xmax) { e.classList.add('hide'); }",
      "      else { zoom_child(e, xmin, ratio); update_text(e); }",
      "    }",
      "  }",
      "  search();",
      "}",
      "function unzoom(dont_update_text) {",
      "  unzoombtn.classList.add('hide');",
      "  var el = document.getElementById('frames').children;",
      "  for(var i = 0; i < el.length; i++) {",
      "    el[i].classList.remove('parent');",
      "    el[i].classList.remove('hide');",
      "    zoom_reset(el[i]);",
      "    if(!dont_update_text) update_text(el[i]);",
      "  }",
      "  search();",
      "}",
      "function clearzoom() {",
      "  unzoom();",
      "  var params = get_params();",
      "  if (params.x) delete params.x;",
      "  if (params.y) delete params.y;",
      "  history.replaceState(null, null, parse_params(params));",
      "}",
      "function toggle_ignorecase() {",
      "  ignorecase = !ignorecase;",
      "  if (ignorecase) { ignorecaseBtn.classList.add('show'); } else { ignorecaseBtn.classList.remove('show'); }",
      "  reset_search();",
      "  search();",
      "}",
      "function reset_search() {",
      "  var el = document.querySelectorAll('#frames rect');",
      "  for (var i = 0; i < el.length; i++) { orig_load(el[i], 'fill') }",
      "  var params = get_params();",
      "  delete params.s;",
      "  history.replaceState(null, null, parse_params(params));",
      "}",
      "function search_prompt() {",
      "  if (!searching) {",
      "    var term = prompt('Enter a search term (regexp allowed, eg: ^ext4_)' + (ignorecase ? ', ignoring case' : '') + '\\nPress Ctrl-i to toggle case sensitivity', '');",
      "    if (term != null) search(term);",
      "  } else {",
      "    reset_search();",
      "    searching = 0;",
      "    currentSearchTerm = null;",
      "    searchbtn.classList.remove('show');",
      "    searchbtn.firstChild.nodeValue = 'Search';",
      "    matchedtxt.classList.add('hide');",
      "    matchedtxt.firstChild.nodeValue = '';",
      "  }",
      "}",
      "function search(term) {",
      "  if (term) currentSearchTerm = term;",
      "  var re = new RegExp(currentSearchTerm, ignorecase ? 'i' : '');",
      "  var el = document.getElementById('frames').children;",
      "  var matches = new Object();",
      "  var maxwidth = 0;",
      "  for (var i = 0; i < el.length; i++) {",
      "    var e = el[i];",
      "    var func = g_to_func(e);",
      "    var rect = find_child(e, 'rect');",
      "    if (func == null || rect == null) continue;",
      "    var w = parseFloat(rect.attributes.width.value);",
      "    if (w > maxwidth) maxwidth = w;",
      "    if (func.match(re)) {",
      "      var x = parseFloat(rect.attributes.x.value);",
      "      orig_save(rect, 'fill');",
      "      rect.attributes.fill.value = '" + SEARCH_COLOR + "';",
      "      if (matches[x] == undefined) { matches[x] = w; } else { if (w > matches[x]) { matches[x] = w; } }",
      "      searching = 1;",
      "    }",
      "  }",
      "  if (!searching) return;",
      "  var params = get_params();",
      "  params.s = currentSearchTerm;",
      "  history.replaceState(null, null, parse_params(params));",
      "  searchbtn.classList.add('show');",
      "  searchbtn.firstChild.nodeValue = 'Reset Search';",
      "  var count = 0;",
      "  var lastx = -1;",
      "  var lastw = 0;",
      "  var keys = Array();",
      "  for (k in matches) { if (matches.hasOwnProperty(k)) keys.push(k); }",
      "  keys.sort(function(a, b){ return a - b; });",
      "  var fudge = 0.0001;",
      "  for (var k in keys) {",
      "    var x = parseFloat(keys[k]);",
      "    var w = matches[keys[k]];",
      "    if (x >= lastx + lastw - fudge) { count += w; lastx = x; lastw = w; }",
      "  }",
      "  matchedtxt.classList.remove('hide');",
      "  var pct = 100 * count / maxwidth;",
      "  if (pct != 100) pct = pct.toFixed(1)",
      "  matchedtxt.firstChild.nodeValue = 'Matched: ' + pct + '%';",
      "}",
    ].join("\n");
  }

  // Render `panels` into a single standalone, interactive flame graph SVG.
  // panels: Array<{ label: string, tree: object }>
  // opts:   { title?: string, width?: number, formatValue?: (count) => string }
  //   formatValue turns a node's weight into the human-readable quantity shown
  //   in each frame's hover text. It defaults to CPU samples ("1,234 samples");
  //   heap exports pass a formatter that renders bytes ("5.2 MB") or allocation
  //   counts ("1,234 allocs") so the SVG matches the on-screen graph.
  function treeToInteractiveSvg(panels, opts) {
    opts = opts || {};
    const W = opts.width || DEFAULT_WIDTH;
    const title = opts.title || "Flame Graph";
    const formatValue =
      typeof opts.formatValue === "function"
        ? opts.formatValue
        : (count) => withCommas(Math.round(count)) + " samples";
    const root = buildExportRoot(panels);

    // Empty graph: still a valid, well-formed SVG with a message.
    if (!root) {
      const h = YPAD1 + YPAD2 + FRAME_HEIGHT;
      return (
        '<?xml version="1.0" standalone="no"?>\n' +
        '<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">\n' +
        `<svg version="1.1" width="${W}" height="${h}" viewBox="0 0 ${W} ${h}" ` +
        'xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">\n' +
        `<rect width="${W}" height="${h}" fill="${BG1}"/>\n` +
        `<text x="${W / 2}" y="${FONT_SIZE * 2}" text-anchor="middle" ` +
        `font-family="${FONT_TYPE}" font-size="${FONT_SIZE + 5}">${escapeXml(title)}</text>\n` +
        `<text x="${XPAD}" y="${FONT_SIZE * 4}" font-family="${FONT_TYPE}" ` +
        `font-size="${FONT_SIZE}">No data to export.</text>\n</svg>\n`
      );
    }

    const timemax = root.count;
    const widthPerTime = (W - 2 * XPAD) / timemax;
    const minTime = MINWIDTH_PX / widthPerTime;
    const { frames, maxDepth } = layoutTree(root, minTime);

    const H = (maxDepth + 1) * FRAME_HEIGHT + YPAD1 + YPAD2;

    const out = [];
    out.push('<?xml version="1.0" standalone="no"?>');
    out.push('<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">');
    out.push(
      `<svg version="1.1" width="${W}" height="${H}" onload="init(evt)" viewBox="0 0 ${W} ${H}" ` +
        'xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">'
    );
    out.push("<!-- Flame graph stack visualization. Exported by dial9. -->");
    out.push("<!-- Structure & embedded script derived from flamegraph.pl, " +
      "https://github.com/brendangregg/FlameGraph -->");
    out.push("<!-- Copyright 2016 Netflix, Inc.; Copyright 2011 Joyent, Inc.; " +
      "Copyright 2011 Brendan Gregg. Licensed under CDDL-1.0. -->");
    out.push("<defs>");
    out.push('<linearGradient id="background" y1="0" y2="1" x1="0" x2="0">');
    out.push(`<stop stop-color="${BG1}" offset="5%"/>`);
    out.push(`<stop stop-color="${BG2}" offset="95%"/>`);
    out.push("</linearGradient>");
    out.push("</defs>");
    out.push('<style type="text/css">');
    out.push(`  text { font-family:${FONT_TYPE}; font-size:${FONT_SIZE}px; fill:rgb(0,0,0); }`);
    out.push("  #search, #ignorecase { opacity:0.1; cursor:pointer; }");
    out.push("  #search:hover, #search.show, #ignorecase:hover, #ignorecase.show { opacity:1; }");
    out.push("  #subtitle { text-anchor:middle; font-color:rgb(160,160,160); }");
    out.push(`  #title { text-anchor:middle; font-size:${FONT_SIZE + 5}px}`);
    out.push("  #unzoom { cursor:pointer; }");
    out.push("  #frames > *:hover { stroke:black; stroke-width:0.5; cursor:pointer; }");
    out.push("  .hide { display:none; }");
    out.push("  .parent { opacity:0.5; }");
    out.push("</style>");
    out.push('<script type="text/ecmascript">');
    out.push("<![CDATA[");
    out.push(embeddedScript());
    out.push("]]>");
    out.push("</script>");

    // Background + chrome.
    out.push(`<rect x="0" y="0" width="${W}" height="${H}" fill="url(#background)"/>`);
    out.push(`<text id="title" x="${(W / 2).toFixed(2)}" y="${FONT_SIZE * 2}">${escapeXml(title)}</text>`);
    out.push(`<text id="details" x="${XPAD}" y="${H - YPAD2 / 2}"> </text>`);
    out.push(`<text id="unzoom" x="${XPAD}" y="${FONT_SIZE * 2}" class="hide">Reset Zoom</text>`);
    out.push(`<text id="search" x="${W - XPAD - 100}" y="${FONT_SIZE * 2}">Search</text>`);
    out.push(`<text id="ignorecase" x="${W - XPAD - 16}" y="${FONT_SIZE * 2}">ic</text>`);
    out.push(`<text id="matched" x="${W - XPAD - 100}" y="${H - YPAD2 / 2}"> </text>`);

    // Frames.
    out.push('<g id="frames">');
    for (const f of frames) {
      const node = f.node;
      // Round x1/x2 to 0.1px FIRST, then derive width from the rounded edges —
      // exactly like flamegraph.pl's filledRectangle. Computing width from the
      // unrounded delta and rounding it independently lets a frame's emitted
      // right edge (x + width) drift from round(x2) by up to 0.1px. The embedded
      // zoom's ancestor test `ex <= xmin && (ex+ew+fudge) >= xmax` uses
      // fudge=0.0001, so a last child sharing its parent's right edge could,
      // after independent double-rounding, push past it and fail the test —
      // leaving the ancestor's context row blank on zoom.
      const rx1 = Number((XPAD + f.sTime * widthPerTime).toFixed(1));
      const rx2 = Number((XPAD + f.eTime * widthPerTime).toFixed(1));
      const x1 = rx1;
      const w = rx2 - rx1;
      const y1 = H - YPAD2 - (f.depth + 1) * FRAME_HEIGHT + FRAME_PAD;
      const y2 = H - YPAD2 - f.depth * FRAME_HEIGHT;
      const h = y2 - y1;

      const isRoot = f.depth === 0;
      const pct = ((100 * node.count) / timemax).toFixed(2);
      const fullName = node.fullName || node.name || "";
      const value = formatValue(node.count);
      const info = isRoot
        ? `all (${value}, 100%)`
        : `${fullName} (${value}, ${pct}%)`;

      // Label text: truncated function name that fits the rect width.
      let label = "";
      if (!isRoot) {
        const chars = Math.floor(w / (FONT_SIZE * FONT_WIDTH));
        if (chars >= 3) {
          const nm = node.name || "";
          label = nm.slice(0, chars);
          if (chars < nm.length) label = label.slice(0, -2) + "..";
        }
      }

      const fill = isRoot ? flamegraphColor("root") : flamegraphColor(node.name || "");
      out.push("<g>");
      out.push(`<title>${escapeXml(info)}</title>`);
      out.push(
        `<rect x="${x1.toFixed(1)}" y="${y1.toFixed(1)}" width="${w.toFixed(1)}" ` +
          `height="${h.toFixed(1)}" fill="${fill}" rx="2" ry="2"/>`
      );
      out.push(
        `<text x="${(x1 + 3).toFixed(2)}" y="${(3 + (y1 + y2) / 2).toFixed(1)}">${escapeXml(label)}</text>`
      );
      out.push("</g>");
    }
    out.push("</g>");
    out.push("</svg>");
    out.push("");
    return out.join("\n");
  }

  // Sanitize an arbitrary label into a safe filename stem. Trims leading/
  // trailing underscores AND dots so we never emit a dotfile (".cache") or a
  // name that is only punctuation (".."); falls back to "flamegraph".
  function filenameStem(label) {
    const base = (label || "flamegraph")
      .replace(/^Flamegraph\s*—\s*/i, "")
      .replace(/[^A-Za-z0-9._-]+/g, "_")
      .replace(/^[._]+|[._]+$/g, "");
    return base || "flamegraph";
  }

  const api = {
    treeToFolded,
    treeToInteractiveSvg,
    buildExportRoot,
    layoutTree,
    filenameStem,
    flamegraphColor,
    escapeXml,
  };
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  } else {
    exports.FlamegraphExport = api;
  }
})(typeof exports === "undefined" ? this : exports);
