"use strict";

// Two-sided differential flamegraph view (the `diff=1` branch of
// flamegraph.html). Renders side A on the LEFT and side B on the RIGHT, each as
// its own self-normalized flamegraph; boxes are colored by relative hotness
// (blue = heavier in A, red = heavier in B) identically in both panels.
//
// This module owns the DOM and the per-side SSE streams. All the value-level
// logic — tree merge, color, per-side layout, zoom-path lookup — is the
// DOM-free, unit-tested core in flamegraph_diff.js. Each side opens its own
// `/api/flamegraph` stream (the server folds to its sampling cap and closes;
// see sse.js), and the merged tree is re-rendered whenever either side pushes
// a new snapshot.
//
// Browser-only (no Node export): the testable pieces live in flamegraph_diff.js.

(function (exports) {
  function getDiff() {
    if (typeof require !== "undefined") return require("./flamegraph_diff.js");
    if (typeof FlamegraphDiff !== "undefined") return FlamegraphDiff;
    throw new Error("FlamegraphDiff not found. Load flamegraph_diff.js first.");
  }
  function getApi() {
    if (typeof require !== "undefined") return require("./flamegraph_api.js");
    return {
      formatCoverageBadge: window.formatCoverageBadge,
      foldErrorNotice: window.foldErrorNotice,
    };
  }
  function getSse() {
    if (typeof require !== "undefined") return require("./sse.js");
    if (typeof Dial9Sse !== "undefined") return Dial9Sse;
    throw new Error("Dial9Sse not found. Load sse.js first.");
  }

  const D = getDiff();
  const ROW = 18; // px per depth level
  // Server flamegraph endpoint keys; the rest of a scope (e.g. the client-only
  // `api` flag) is not forwarded.
  const SERVER_KEYS = [
    "data_dir", "bucket", "prefix", "service",
    "thread_class", "source", "spawn_location", "start_ns", "end_ns", "max_files",
  ];

  // Build the `/api/flamegraph` URL for one side from its scope params. The
  // endpoint is an SSE stream (the server owns refinement — no client `refine`
  // flag). `origin` defaults to the page origin in the browser; tests pass it in.
  function apiUrlFor(scope, origin) {
    const base = origin || (typeof window !== "undefined" ? window.location.origin : "http://localhost");
    const u = new URL("/api/flamegraph", base);
    for (const k of SERVER_KEYS) {
      const v = scope.get(k);
      if (v != null && v !== "") u.searchParams.set(k, v);
    }
    for (const h of scope.getAll("host")) u.searchParams.append("host", h);
    return u;
  }

  // Short human label for one side from its scope (service @ host, host count).
  function scopeLabel(scope, fallback) {
    const svc = scope.get("service");
    const hosts = scope.getAll("host");
    let s = svc || fallback;
    if (hosts.length === 1) s += " @ " + hosts[0];
    else if (hosts.length > 1) s += " @ " + hosts.length + " hosts";
    return s;
  }

  // createDiffView(container, opts)
  //   opts.scopeA / opts.scopeB — URLSearchParams of each side's scope
  //   opts.headersFor(side)     — returns the credential headers for "a"/"b"
  //   opts.onSideError(side, e) — optional; called on a side's fetch failure
  //                               (used to drive the per-side BYOC prompt)
  function createDiffView(container, opts) {
    const api = getApi();
    const scopeA = opts.scopeA;
    const scopeB = opts.scopeB;
    const headersFor = opts.headersFor || (() => ({}));
    const onSideError = opts.onSideError || (() => {});
    const labelA = scopeLabel(scopeA, "A");
    const labelB = scopeLabel(scopeB, "B");

    // ── DOM scaffold ──
    container.innerHTML = "";
    container.style.display = "flex";
    container.style.flexDirection = "column";
    container.style.flex = "1";
    container.style.overflow = "hidden";

    const header = document.createElement("div");
    header.className = "fgd-header";
    header.innerHTML =
      '<input class="fgd-search" placeholder="highlight frames (regex)…" />' +
      '<button class="fgd-reset">Reset zoom</button>' +
      '<span class="fgd-hotness" title="Overall sampling rate difference. Colors normalize this away (shape vs shape); this is the absolute volume signal."></span>' +
      '<div class="fgd-legend"><span class="fgd-leg-a"></span>' +
      '<span class="fgd-bar"></span><span class="fgd-leg-b"></span></div>';
    container.appendChild(header);
    const searchInput = header.querySelector(".fgd-search");
    const resetBtn = header.querySelector(".fgd-reset");
    const hotnessEl = header.querySelector(".fgd-hotness");
    header.querySelector(".fgd-leg-a").textContent = "◀ heavier in " + labelA;
    header.querySelector(".fgd-leg-b").textContent = "heavier in " + labelB + " ▶";

    const breadcrumb = document.createElement("div");
    breadcrumb.className = "fgd-breadcrumb";
    breadcrumb.style.display = "none";
    container.appendChild(breadcrumb);

    const row = document.createElement("div");
    row.className = "fgd-row";
    container.appendChild(row);

    // Each panel renders to a single <canvas> (not a div-per-frame): a deep
    // prod flamegraph is thousands of frames × two panels, and retained-mode
    // DOM (divs or SVG) buckles under that node count + per-node listeners.
    // Canvas keeps it to one element per panel, an array hit-test, and a
    // rAF-coalesced repaint — the same approach as the single-flamegraph
    // renderer in flamegraph.js.
    function makePanel(cls) {
      const panel = document.createElement("div");
      panel.className = "fgd-panel";
      const lbl = document.createElement("div");
      lbl.className = "fgd-panel-label " + cls;
      panel.appendChild(lbl);
      const graph = document.createElement("div");
      graph.className = "fgd-graph";
      const canvas = document.createElement("canvas");
      graph.appendChild(canvas);
      panel.appendChild(graph);
      row.appendChild(panel);
      return { panel: panel, label: lbl, graph: graph, canvas: canvas };
    }
    const panelA = makePanel("a");
    const panelB = makePanel("b");

    const tip = document.createElement("div");
    tip.className = "fgd-tip";
    document.body.appendChild(tip);

    // ── State ──
    let treeA = null; // server JSON tree, side A
    let treeB = null; // server JSON tree, side B
    let merged = D.mergeTrees(null, null);
    let zoomPath = ["(all)"];
    const SEP = String.fromCharCode(31);
    const statusA = { total: 0, badge: "", meta: null };
    const statusB = { total: 0, badge: "", meta: null };
    // Cached per-side layouts ({ boxes, maxDepth }); recomputed only on
    // data/zoom/resize, NOT on hover. Each box is augmented with a stable
    // `key` (path join) and precomputed `color` so repaint is pure drawing.
    const layoutCache = { a: null, b: null };
    let hoverKey = null; // path key highlighted in both panels
    let searchRe = null; // active highlight regex, or null
    let tipW = 0, tipH = 0; // cached tooltip size; remeasured only on content change

    function rootName() {
      return (treeA && treeA.name) || (treeB && treeB.name) || "(all)";
    }

    // ── Layout (data → drawable boxes) ──
    function buildLayout(focus, side) {
      const p = side === "a" ? panelA : panelB;
      const widthPx = p.graph.clientWidth || (row.clientWidth / 2) || 600;
      const layout = D.layoutSide(focus, zoomPath, side, widthPx);
      for (const box of layout.boxes) {
        box.key = box.path.join(SEP);
        // Shared color: blue = heavier in A, red = heavier in B (same in both
        // panels — only width differs). Normalized to each side's global total.
        box.color = D.diffColor(box.a, box.b, merged.a, merged.b);
      }
      return layout;
    }

    // Full render: re-merge, relayout both sides, paint. Called on new data,
    // zoom change, and resize.
    function render() {
      merged = D.mergeTrees(treeA, treeB);
      // Resolve the zoom focus; if a prior zoom path no longer exists in the
      // merged tree (pruned away), fall back to the root.
      let focus = D.nodeAtPath(merged, zoomPath);
      if (!focus) {
        zoomPath = [rootName()];
        focus = merged;
      }
      layoutCache.a = buildLayout(focus, "a");
      layoutCache.b = buildLayout(focus, "b");
      renderBreadcrumb();
      paint("a");
      paint("b");
    }

    // ── Paint (drawable boxes → canvas) ──
    function sizeCanvas(canvas, cssW, cssH) {
      const dpr = window.devicePixelRatio || 1;
      canvas.width = Math.max(1, Math.round(cssW * dpr));
      canvas.height = Math.max(1, Math.round(cssH * dpr));
      canvas.style.width = cssW + "px";
      canvas.style.height = cssH + "px";
      const ctx = canvas.getContext("2d");
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      return ctx;
    }

    function paint(side) {
      const p = side === "a" ? panelA : panelB;
      const cache = layoutCache[side];
      const cssW = p.graph.clientWidth || (row.clientWidth / 2) || 600;
      const cssH = (cache ? (cache.maxDepth + 1) * ROW : ROW) + 2;
      p.graph.style.height = cssH + "px";
      const ctx = sizeCanvas(p.canvas, cssW, cssH);
      ctx.fillStyle = "#1a1a2e";
      ctx.fillRect(0, 0, cssW, cssH);
      if (!cache) return;
      ctx.font = "11px monospace";
      ctx.textBaseline = "middle";
      for (const box of cache.boxes) {
        const y = box.depth * ROW;
        const w = Math.max(box.w - 1, 0.5);
        const faded = searchRe != null && !searchRe.test(box.name);
        ctx.globalAlpha = faded ? 0.15 : 1;
        ctx.fillStyle = box.color;
        ctx.fillRect(box.x, y, w, ROW - 1);
        if (box.key === hoverKey) {
          ctx.globalAlpha = 1;
          ctx.strokeStyle = "#fff";
          ctx.lineWidth = 1;
          ctx.strokeRect(box.x + 0.5, y + 0.5, Math.max(w - 1, 1), ROW - 2);
        }
        if (box.w > 30) {
          ctx.globalAlpha = faded ? 0.3 : 1;
          ctx.fillStyle = "#111";
          ctx.save();
          ctx.beginPath();
          ctx.rect(box.x + 2, y, w - 4, ROW);
          ctx.clip();
          ctx.fillText(box.name, box.x + 3, y + ROW / 2);
          ctx.restore();
        }
      }
      ctx.globalAlpha = 1;
    }

    // Coalesce hover-driven repaints to one per animation frame.
    let paintQueued = false;
    function repaint() {
      if (paintQueued) return;
      paintQueued = true;
      requestAnimationFrame(() => {
        paintQueued = false;
        paint("a");
        paint("b");
      });
    }

    // ── Hit-testing & interaction (canvas-level, not per-frame) ──
    function boxAtEvent(side, e) {
      const cache = layoutCache[side];
      if (!cache) return null;
      const p = side === "a" ? panelA : panelB;
      const r = p.canvas.getBoundingClientRect();
      const mx = e.clientX - r.left, my = e.clientY - r.top;
      for (const box of cache.boxes) {
        const y = box.depth * ROW;
        if (mx >= box.x && mx <= box.x + box.w && my >= y && my < y + ROW) return box;
      }
      return null;
    }

    function onPanelMove(side, e) {
      const p = side === "a" ? panelA : panelB;
      const box = boxAtEvent(side, e);
      if (!box) {
        if (hoverKey !== null) { hoverKey = null; repaint(); }
        hideTip();
        p.canvas.style.cursor = "";
        return;
      }
      p.canvas.style.cursor = "pointer";
      // Rebuild tooltip content + remeasure ONLY when the frame changes; plain
      // movement just repositions using the cached size (no forced reflow).
      if (box.key !== hoverKey) {
        hoverKey = box.key;
        showTipContent(box);
        repaint();
      }
      positionTip(e.clientX, e.clientY);
    }

    function setupPanelEvents(side) {
      const p = side === "a" ? panelA : panelB;
      p.canvas.addEventListener("mousemove", (e) => onPanelMove(side, e));
      p.canvas.addEventListener("mouseleave", () => {
        if (hoverKey !== null) { hoverKey = null; repaint(); }
        hideTip();
      });
      p.canvas.addEventListener("click", (e) => {
        const box = boxAtEvent(side, e);
        if (box && box.path.length > zoomPath.length) { zoomPath = box.path.slice(); render(); }
      });
      p.canvas.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        if (zoomPath.length > 1) { zoomPath = zoomPath.slice(0, -1); render(); }
      });
    }
    setupPanelEvents("a");
    setupPanelEvents("b");

    function renderBreadcrumb() {
      if (zoomPath.length <= 1) { breadcrumb.style.display = "none"; return; }
      breadcrumb.style.display = "flex";
      breadcrumb.innerHTML = "";
      for (let i = 0; i < zoomPath.length; i++) {
        if (i > 0) {
          const sep = document.createElement("span");
          sep.className = "fgd-bc-sep";
          sep.textContent = " › ";
          breadcrumb.appendChild(sep);
        }
        const span = document.createElement("span");
        const isLast = i === zoomPath.length - 1;
        span.className = "fgd-bc-item" + (isLast ? "" : " fgd-bc-link");
        span.textContent = zoomPath[i];
        if (!isLast) {
          const idx = i;
          span.addEventListener("click", () => { zoomPath = zoomPath.slice(0, idx + 1); render(); });
        }
        breadcrumb.appendChild(span);
      }
    }

    // ── Tooltip ──
    // Split into content (rebuild + remeasure, only when the hovered frame
    // changes) and position (cheap, on every move). The old code rebuilt
    // innerHTML AND read offsetWidth/Height on every mousemove, forcing a
    // synchronous reflow per pixel — the main source of the hover lag.
    function fmtPct(x) {
      if (!isFinite(x)) return "0%";
      return (x * 100 < 0.01 && x > 0 ? "<0.01" : (x * 100).toFixed(2)) + "%";
    }
    function showTipContent(box) {
      const fa = merged.a ? box.a / merged.a : 0;
      const fb = merged.b ? box.b / merged.b : 0;
      let ratio;
      if (box.a === 0 && box.b === 0) ratio = "—";
      else if (box.a === 0) ratio = "∞ (" + labelB + " only)";
      else if (box.b === 0) ratio = "∞ (" + labelA + " only)";
      else ratio = (fb / fa).toFixed(2) + "×";
      const nameEl = document.createElement("div");
      nameEl.className = "fgd-tip-fn";
      nameEl.textContent = box.name;
      tip.innerHTML = "";
      tip.appendChild(nameEl);
      const tbl = document.createElement("table");
      tbl.innerHTML =
        '<tr><td class="av">' + esc(labelA) + '</td><td class="num av">' +
          box.a.toLocaleString() + '</td><td class="num av">' + fmtPct(fa) + "</td></tr>" +
        '<tr><td class="bv">' + esc(labelB) + '</td><td class="num bv">' +
          box.b.toLocaleString() + '</td><td class="num bv">' + fmtPct(fb) + "</td></tr>" +
        '<tr><td class="delta">' + esc(labelB) + "/" + esc(labelA) +
          '</td><td class="num delta" colspan="2">' + esc(ratio) + "</td></tr>";
      tip.appendChild(tbl);
      tip.style.display = "block";
      // Measure once here (the only forced layout), cache for positioning.
      tipW = tip.offsetWidth;
      tipH = tip.offsetHeight;
    }
    function positionTip(cx, cy) {
      let x = cx + 14, y = cy + 14;
      if (x + tipW > innerWidth) x = cx - tipW - 14;
      if (y + tipH > innerHeight) y = cy - tipH - 14;
      tip.style.left = x + "px";
      tip.style.top = y + "px";
    }
    function hideTip() { tip.style.display = "none"; }
    function esc(s) {
      const d = document.createElement("span");
      d.textContent = String(s);
      return d.innerHTML;
    }

    // ── Search ──
    // Recompiles the highlight regex and repaints; matched/faded state is drawn
    // by paint() (per-box alpha), not per-element class toggles.
    function applySearch() {
      const q = searchInput.value.trim();
      searchRe = null;
      if (q) { try { searchRe = new RegExp(q, "i"); } catch (e) { searchRe = null; } }
      repaint();
    }
    searchInput.addEventListener("input", applySearch);
    resetBtn.addEventListener("click", () => { zoomPath = [rootName()]; render(); });
    window.addEventListener("resize", render);
    window.addEventListener("keydown", (e) => {
      if (e.key === "Escape") { zoomPath = [rootName()]; render(); }
    });

    // ── Header stats ──
    // The "sampled window" for a side is its actual time span: prefer the
    // scope's explicit start/end, else the metadata min/max sample timestamps.
    // Returns { fromMs, toMs, durNs } or null if unknown.
    function sampledWindow(scope, meta) {
      let startNs = scope.get("start_ns");
      let endNs = scope.get("end_ns");
      startNs = startNs != null ? Number(startNs) : (meta && meta.min_timestamp_ns);
      endNs = endNs != null ? Number(endNs) : (meta && meta.max_timestamp_ns);
      if (startNs == null || endNs == null || !(endNs > startNs)) return null;
      return { fromMs: startNs / 1e6, toMs: endNs / 1e6, durNs: endNs - startNs };
    }

    // samples / minute / host — the rate that reveals "this side is generally
    // hotter" independent of window length or fleet size. null if unknowable.
    function sampleRatePerMinHost(total, win, meta) {
      if (!win || !win.durNs) return null;
      const minutes = win.durNs / 60e9;
      const hosts = (meta && meta.hosts) || 1;
      if (minutes <= 0 || hosts <= 0) return null;
      return total / minutes / hosts;
    }

    // Build the two-line label for one panel: scope line + volume line.
    function panelStats(scope, status) {
      const meta = status.meta;
      const win = sampledWindow(scope, meta);
      const scopeBits = [];
      const svc = scope.get("service");
      if (svc) scopeBits.push(svc);
      const hosts = scope.getAll("host").length || (meta && meta.hosts) || 0;
      if (hosts) scopeBits.push(hosts + (hosts === 1 ? " host" : " hosts"));
      if (win) {
        const from = new Date(win.fromMs).toISOString().slice(5, 16).replace("T", " ");
        scopeBits.push(from + " UTC · " + formatHumanDuration(win.durNs));
      }
      const tc = scope.get("thread_class");
      if (tc) scopeBits.push(tc);
      const src = scope.get("source");
      if (src && src !== "cpu") scopeBits.push(src);

      const volBits = [status.total.toLocaleString() + " samples"];
      const rate = sampleRatePerMinHost(status.total, win, meta);
      if (rate != null) volBits.push(Math.round(rate).toLocaleString() + " samples/min/host");
      if (status.badge) volBits.push(status.badge);

      return { scope: scopeBits.join("  ·  "), vol: volBits.join("  ·  "), rate: rate };
    }

    function updateStats() {
      const a = panelStats(scopeA, statusA);
      const b = panelStats(scopeB, statusB);
      // Two lines per panel: scope (what) on top, volume (how much) below.
      panelA.label.innerHTML =
        '<span class="fgd-pl-scope">' + esc(labelA) + " — " + esc(a.scope) + "</span>" +
        '<span class="fgd-pl-vol">' + esc(a.vol) + "</span>";
      panelB.label.innerHTML =
        '<span class="fgd-pl-scope">' + esc(labelB) + " — " + esc(b.scope) + "</span>" +
        '<span class="fgd-pl-vol">' + esc(b.vol) + "</span>";

      // Cross-side hotness: ratio of sample rates (volume signal the colors
      // deliberately normalize away). Only shown once both rates are known and
      // they differ by >10%.
      if (a.rate != null && b.rate != null && a.rate > 0 && b.rate > 0) {
        const r = b.rate / a.rate;
        if (r >= 1.1) {
          hotnessEl.textContent = "🔥 " + labelB + " ~" + r.toFixed(1) + "× hotter (samples/min/host)";
          hotnessEl.style.display = "";
        } else if (r <= 1 / 1.1) {
          hotnessEl.textContent = "🔥 " + labelA + " ~" + (1 / r).toFixed(1) + "× hotter (samples/min/host)";
          hotnessEl.style.display = "";
        } else {
          hotnessEl.textContent = "≈ same sampling rate";
          hotnessEl.style.display = "";
        }
      } else {
        hotnessEl.style.display = "none";
      }
    }

    // ── Per-side SSE stream ──
    // One stream per side: the server emits the already-folded snapshot, then a
    // fresh full snapshot per newly-folded file, and closes at its sampling cap
    // (it owns the stop condition — no client polling or plateau detection). A
    // new tree on either side triggers a re-merge + re-render.
    function startSide(side, scope, status) {
      const sse = getSse();
      const ctl = new AbortController();

      sse.openSse(apiUrlFor(scope), {
        headers: headersFor(side),
        signal: ctl.signal,
        onEvent: (resp) => {
          if (side === "a") treeA = resp.tree; else treeB = resp.tree;
          status.total = resp.total_samples || (resp.tree && resp.tree.count) || 0;
          status.meta = resp.metadata || null;
          const cov = resp.coverage;
          if (cov != null) {
            status.badge = api.formatCoverageBadge(cov) + " · refining…";
            // Surface fold failures (e.g. an unwritable output bucket) instead
            // of letting a side silently render an empty or shallow tree.
            const notice = api.foldErrorNotice(cov);
            if (notice) status.badge += " · " + notice;
          } else {
            status.badge = "";
          }
          updateStats();
          render();
        },
        onClose: () => {
          // Server folded this side to its cap and closed the stream.
          status.badge = status.badge.replace(" · refining…", " · refined");
          updateStats();
        },
        onError: (e) => onSideError(side, e),
      });
      return { stop() { ctl.abort(); } };
    }

    // `let` (not `const`): repollSide replaces the handle so destroy() always
    // aborts the *current* stream, and a second re-prompt cancels the prior
    // retry rather than leaving two streams open for the same side.
    let sideA = startSide("a", scopeA, statusA);
    let sideB = startSide("b", scopeB, statusB);

    // Abort and reopen one side's stream (used after the user supplies B's
    // creds). Already-folded files are re-served instantly, so a retry is cheap.
    function repollSide(side) {
      if (side === "a") { sideA.stop(); sideA = startSide("a", scopeA, statusA); }
      else { sideB.stop(); sideB = startSide("b", scopeB, statusB); }
    }

    updateStats();
    render();

    return {
      destroy() {
        sideA.stop();
        sideB.stop();
        if (tip.parentNode) tip.parentNode.removeChild(tip);
      },
      repollSide,
      labels: { a: labelA, b: labelB },
    };
  }

  const ex = { createDiffView, apiUrlFor, scopeLabel };
  if (typeof module !== "undefined" && module.exports) module.exports = ex;
  else exports.FlamegraphDiffView = ex;
})(typeof exports === "undefined" ? this : exports);
