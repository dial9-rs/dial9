// flamegraph.js - Shared flamegraph rendering with zoom and search

(function (exports) {
  "use strict";

  function getAnalysis() {
    if (typeof require !== "undefined") return require("./trace_analysis.js");
    if (typeof TraceAnalysis !== "undefined") return TraceAnalysis;
    throw new Error(
      "TraceAnalysis not found. Load trace_analysis.js before flamegraph.js"
    );
  }

  function getExport() {
    if (typeof require !== "undefined") return require("./flamegraph_export.js");
    if (typeof FlamegraphExport !== "undefined") return FlamegraphExport;
    return null; // export is optional; UI degrades gracefully if not loaded
  }

  const buildFlamegraphTree = getAnalysis().buildFlamegraphTree;
  const buildRuntimeFilterData = getAnalysis().buildRuntimeFilterData;
  // Shared with the SVG export (flamegraph_export.js) via trace_analysis.js so
  // the exported graph's colors match the on-screen canvas.
  const flamegraphColor = getAnalysis().flamegraphColor;
  const FG_ROW_H = 18;

  // Trigger a browser download of `content` (string) as `filename`.
  function downloadFile(filename, content, mime) {
    const blob = new Blob([content], { type: mime || "application/octet-stream" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    // Revoke on the next tick so the click has been dispatched.
    setTimeout(() => URL.revokeObjectURL(url), 0);
  }

  // Like flattenFlamegraph in trace_analysis.js but attaches treeNode refs
  // for click-to-zoom. Filters out nodes < 0.1% of total.
  // `ancestors` is an optional array of tree nodes forming the parent chain
  // from the tree root down to (but not including) `root`. When provided,
  // they are rendered as full-width context bars below the zoom target.
  function flattenFromNode(root, total, ancestors) {
    ancestors = ancestors || [];
    const nodes = [];
    let maxD = 0;
    const depthOffset = ancestors.length;
    const zoomed = depthOffset > 0;

    // Ancestor chain: full-width context bars at depths 0..N-1
    for (let i = 0; i < ancestors.length; i++) {
      nodes.push({
        name: ancestors[i].name, depth: i, x: 0, w: 1,
        count: ancestors[i].count, self: ancestors[i].self,
        treeNode: ancestors[i], isAncestor: true,
      });
      if (i > maxD) maxD = i;
    }

    // Zoom target itself at depth N (full-width)
    if (zoomed) {
      nodes.push({
        name: root.name, depth: depthOffset, x: 0, w: 1,
        count: root.count, self: root.self, treeNode: root,
      });
      if (depthOffset > maxD) maxD = depthOffset;
    }

    const startDepth = zoomed ? depthOffset + 1 : 0;
    function walk(treeNode, depth, xStart) {
      const w = treeNode.count / total;
      if (w < 0.001) return;
      nodes.push({
        name: treeNode.name,
        depth,
        x: xStart,
        w,
        count: treeNode.count,
        self: treeNode.self,
        treeNode,
      });
      if (depth > maxD) maxD = depth;
      const kids = [...treeNode.children.values()].sort(
        (a, b) => b.count - a.count
      );
      let cx = xStart;
      for (const child of kids) {
        walk(child, depth + 1, cx);
        cx += child.count / total;
      }
    }
    const kids = [...root.children.values()].sort(
      (a, b) => b.count - a.count
    );
    let cx = 0;
    for (const child of kids) {
      walk(child, startDepth, cx);
      cx += child.count / total;
    }
    return { nodes, maxDepth: maxD };
  }

  // Aggregate stats for the compact "N frames · X% of samples" line next to the
  // search box. Kept consistent with the results dropdown and the inspect focus
  // band, which both report INCLUSIVE weight:
  //   - functions: distinct matching functions (by frameKey), matching the
  //     dropdown's "N matching frames" — not raw tree-node count.
  //   - covered:   inclusive sample union — samples whose stack contains at
  //     least one match, counted once at the top-most match on each stack so
  //     nested matches aren't double-counted. For a single matching function
  //     this equals that frame's inclusive total (what inspect shows).
  function searchAggregate(roots, queryLower) {
    const rootList = Array.isArray(roots) ? roots : [roots];
    const fns = new Set();
    let covered = 0;
    let rootTotal = 0;
    function walk(node, ancestorMatched) {
      const matched =
        node.name.toLowerCase().includes(queryLower) ||
        (node.fullName && node.fullName.toLowerCase().includes(queryLower));
      if (matched) {
        fns.add(node.fullName || node.name);
        if (!ancestorMatched) covered += node.count;
      }
      const childAncestorMatched = ancestorMatched || matched;
      for (const child of node.children.values()) walk(child, childAncestorMatched);
    }
    for (const root of rootList) {
      if (!root) continue;
      rootTotal += root.count || 0;
      for (const child of root.children.values()) walk(child, false);
    }
    return { functions: fns.size, covered: covered, rootTotal: rootTotal };
  }

  // ── Inspect / butterfly (issue #652) ────────────────────────────────────
  // Two frames are "the same function" when their identity key matches. The
  // exact-trace tree keys children by symbol (fullName); aggregated/API trees
  // (built by toFgTree) have no fullName, so fall back to the display name.
  function frameKey(node) {
    return node.fullName || node.name;
  }

  // Fresh inspect-tree node, copying the display metadata (location/docsUrl) of
  // a source tree node so tooltips and the Ctrl-click docs link keep working.
  function newInspectNode(name, fullName, src) {
    return {
      name: name,
      fullName: fullName || null,
      location: src ? src.location || null : null,
      docsUrl: src ? src.docsUrl || null : null,
      count: 0,
      self: 0,
      children: new Map(),
    };
  }

  // Deep-merge `src`'s subtree into `dst` (both inspect nodes), accumulating
  // counts. Used to aggregate the callee subtrees of every occurrence of the
  // focus frame into one downward "code called by <focus>" tree.
  function mergeSubtree(dst, src) {
    dst.count += src.count;
    dst.self += src.self;
    for (const child of src.children.values()) {
      const k = frameKey(child);
      let d = dst.children.get(k);
      if (!d) {
        d = newInspectNode(child.name, child.fullName, child);
        dst.children.set(k, d);
      }
      mergeSubtree(d, child);
    }
  }

  // Build the butterfly ("inspect") view of `focus` across the whole `root`
  // tree. Returns two trees, both rooted at the focus function:
  //   - callees: code called *by* focus (paths out of it), merged over every
  //     occurrence. Rendered growing up, above the focus band.
  //   - callers: the inverted caller chains (paths *into* focus), each
  //     occurrence contributing its own weight. Rendered growing down.
  // Recursion is handled by only treating the *top-most* occurrence on each
  // stack as a call site (so focus-calls-focus is not double-counted); nested
  // self-samples are still tallied so `self` stays exact.
  // `roots` may be a single tree root or an array of roots (e.g. the worker and
  // off-worker lanes), letting the butterfly span the whole profile.
  function buildInspect(roots, focus) {
    const rootList = Array.isArray(roots) ? roots : [roots];
    const focusKey = frameKey(focus);
    const focusName = focus.name;
    const focusFullName = focus.fullName || null;
    const occurrences = [];
    let inclusiveTotal = 0;
    let selfTotal = 0;
    let rootTotal = 0;
    const path = [];
    let inside = 0;
    function dfs(node) {
      const matched = frameKey(node) === focusKey;
      path.push(node);
      if (matched) {
        selfTotal += node.self;
        if (inside === 0) {
          occurrences.push({ node: node, path: path.slice() });
          inclusiveTotal += node.count;
        }
        inside++;
      }
      for (const child of node.children.values()) dfs(child);
      if (matched) inside--;
      path.pop();
    }
    // Iterate each root's children so the synthetic "(all)" root is not treated
    // as a caller (it would just add a full-width, meaningless band). The shared
    // `inside`/`path` counters return to their base between roots because every
    // push is matched by a pop, so sequential DFS over multiple roots is safe.
    for (const root of rootList) {
      if (!root) continue;
      rootTotal += root.count || 0;
      for (const child of root.children.values()) dfs(child);
    }

    const callees = newInspectNode(focusName, focusFullName, focus);
    for (const occ of occurrences) mergeSubtree(callees, occ.node);
    callees.self = selfTotal; // exact self incl. nested recursion

    const callers = newInspectNode(focusName, focusFullName, focus);
    callers.count = inclusiveTotal;
    callers.self = selfTotal;
    for (const occ of occurrences) {
      const p = occ.path; // [outermostCaller, ..., focus]
      const c = occ.node.count;
      let cur = callers;
      for (let i = p.length - 2; i >= 0; i--) {
        const anc = p[i];
        const k = frameKey(anc);
        let d = cur.children.get(k);
        if (!d) {
          d = newInspectNode(anc.name, anc.fullName, anc);
          cur.children.set(k, d);
        }
        d.count += c;
        cur = d;
      }
    }

    return {
      focusName: focusName,
      focusFullName: focusFullName,
      focusKey: focusKey,
      location: focus.location || null,
      docsUrl: focus.docsUrl || null,
      callers: callers,
      callees: callees,
      total: inclusiveTotal,
      self: selfTotal,
      rootTotal: rootTotal,
      occurrences: occurrences.length,
    };
  }

  // Collect search matches grouped by function (issue #653). Each result is one
  // function whose display name or symbol contains `queryLower`, with:
  //   - total: inclusive samples with the function anywhere on the stack
  //     (counted once per stack, at the top-most occurrence — no recursion
  //     double-count), i.e. how "big" the frame is across the whole graph.
  //   - self:  leaf samples attributed to the function (always exact).
  //   - sites: number of distinct call paths (top-most occurrences) into the
  //     function; rendered as "call paths" in the UI.
  function collectSearchResults(roots, queryLower) {
    const rootList = Array.isArray(roots) ? roots : [roots];
    const byKey = new Map();
    const active = new Map(); // frameKey -> nesting depth on current path
    function walk(node) {
      const matched =
        node.name.toLowerCase().includes(queryLower) ||
        (node.fullName && node.fullName.toLowerCase().includes(queryLower));
      let k = null;
      if (matched) {
        k = frameKey(node);
        let e = byKey.get(k);
        if (!e) {
          e = {
            key: k,
            name: node.name,
            fullName: node.fullName || null,
            location: node.location || null,
            docsUrl: node.docsUrl || null,
            total: 0,
            self: 0,
            sites: 0,
          };
          byKey.set(k, e);
        }
        e.self += node.self;
        const act = active.get(k) || 0;
        if (act === 0) {
          e.total += node.count;
          e.sites++;
        }
        active.set(k, act + 1);
      }
      for (const child of node.children.values()) walk(child);
      if (matched) active.set(k, active.get(k) - 1);
    }
    // Walk each root's children (skip the synthetic "(all)" root itself so it is
    // never a search hit). `active` returns to base between roots by pop-parity.
    for (const root of rootList) {
      if (!root) continue;
      for (const child of root.children.values()) walk(child);
    }
    return [...byKey.values()];
  }

  function filterCpuSamples(cpuSamples, startNs, endNs) {
    let out = cpuSamples.filter((s) => s.callchain.length > 0 && s.source !== 1);
    if (startNs != null) out = out.filter((s) => s.timestamp >= startNs);
    if (endNs != null) out = out.filter((s) => s.timestamp <= endNs);
    return out;
  }

  function createFlamegraph(container, onZoomChange) {
    onZoomChange = onZoomChange || function () {};
    // While restoring view state from a URL we mutate zoom/inspect/search/filters
    // programmatically; those must NOT fire the persist callback (which would
    // rewrite the address bar mid-restore, and — via writeState's delete-on-
    // absence — could clobber a URL key the restore hasn't applied yet). All
    // internal "view changed" notifications go through notifyChange() so restore
    // can suspend them; genuine user interactions run with it un-suspended.
    let suspendNotify = false;
    function notifyChange() {
      if (!suspendNotify) onZoomChange();
    }
    let workerTree = null;
    let offworkerTree = null;
    let workerData = null;
    let offworkerData = null;
    let workerZoomStack = [];
    let offworkerZoomStack = [];
    let searchQuery = "";
    let highlightName = null;
    let repaintQueued = false;
    let allSamples = [];
    let currentSymbols = null;
    // True once setTreeDirect() has installed a pre-built (aggregated/API) tree.
    // In that mode there are no raw `allSamples`, so the spawn/runtime filters
    // (which rebuild the trees from samples) do not apply and must not run —
    // applyFilters() over an empty sample set would wipe the direct-set tree.
    let directMode = false;
    // Map of workerId -> runtime name, derived from the trace's runtime.<name>
    // segment metadata. Empty when the trace has a single runtime (or none),
    // in which case the runtime filter stays hidden.
    let workerRuntime = new Map();
    const hitRegions = { worker: [], offworker: [], callees: [], callers: [] };

    // ── Inspect / butterfly state (issue #652) ──
    // When active, the normal worker/off-worker canvases are hidden and the
    // butterfly (callers below, callees above a focus band) takes over. The
    // focus always spans the FULL trees (not the current zoom), so inspect
    // answers "all paths into/out of this frame across the entire flamegraph".
    let inspectActive = false;
    let inspectFocusSrc = null; // the source-tree node currently focused
    let inspectResult = null; // buildInspect() output for inspectFocusSrc
    let inspectCalleesData = null; // flattened callees (grows up)
    let inspectCallersData = null; // flattened callers (grows down)
    let inspectHistory = []; // pivot trail of prior focus nodes (for back/breadcrumb)
    let pushInspectScroll = false; // center the focus band on the next inspect render

    // DOM
    const searchBar = document.createElement("div");
    searchBar.className = "fg-search-bar";
    const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
    searchBar.innerHTML =
      '<input type="text" class="fg-search-input" placeholder="Search frames... (' +
      (isMac ? '\u2318' : 'Ctrl') + ' + F or /)" />' +
      '<span class="fg-search-clear" title="Clear search">\u00d7</span>' +
      '<span class="fg-search-stats"></span>' +
      '<select class="fg-runtime-filter" style="display:none"></select>' +
      '<select class="fg-spawn-filter"></select>' +
      '<span class="fg-export-wrap">' +
      '<button type="button" class="fg-export-btn" title="Export this flamegraph" ' +
      'aria-haspopup="menu" aria-expanded="false" disabled>\u2b07 Export</button>' +
      '<div class="fg-export-menu" role="menu" style="display:none">' +
      '<button type="button" role="menuitem" class="fg-export-svg">Interactive SVG (.svg)</button>' +
      '<button type="button" role="menuitem" class="fg-export-folded">Folded stacks (.txt)</button>' +
      '</div>' +
      '</span>' +
      '<span class="fg-help-btn" tabindex="0" role="button" title="Keyboard shortcuts">\u2139\ufe0f</span>';
    container.appendChild(searchBar);

    const searchInput = searchBar.querySelector(".fg-search-input");
    const searchClear = searchBar.querySelector(".fg-search-clear");
    const searchStats = searchBar.querySelector(".fg-search-stats");
    const spawnFilter = searchBar.querySelector(".fg-spawn-filter");
    const runtimeFilter = searchBar.querySelector(".fg-runtime-filter");
    const exportBtn = searchBar.querySelector(".fg-export-btn");
    const exportMenu = searchBar.querySelector(".fg-export-menu");
    const exportSvgBtn = searchBar.querySelector(".fg-export-svg");
    const exportFoldedBtn = searchBar.querySelector(".fg-export-folded");
    const helpBtn = searchBar.querySelector(".fg-help-btn");

    const helpOverlay = document.createElement("div");
    helpOverlay.className = "fg-help-overlay";
    helpOverlay.innerHTML =
      '<div class="fg-help-content">' +
      '<h3>\u2328 Flamegraph Shortcuts</h3>' +
      '<table>' +
      '<tr><td class="fg-help-key">Click</td><td>Zoom into frame (or re-pivot while inspecting)</td></tr>' +
      '<tr><td class="fg-help-key">Option / Alt + click</td><td>Pin tooltip (selectable text, links)</td></tr>' +
      '<tr><td class="fg-help-key">' + (isMac ? '\u2318' : 'Ctrl') + ' + click</td><td>Open docs.rs (when available)</td></tr>' +
      '<tr><td class="fg-help-key">Right-click</td><td>Menu: Inspect frame / Zoom out / Copy name</td></tr>' +
      '<tr><td class="fg-help-key">Inspect</td><td>All paths into (below) &amp; out of (above) a frame</td></tr>' +
      '<tr><td class="fg-help-key">' + (isMac ? '\u2318' : 'Ctrl') + ' + F or /</td><td>Search frames \u2192 click a result to inspect</td></tr>' +
      '<tr><td class="fg-help-key">Esc</td><td>Unpin \u2192 close menu \u2192 clear search \u2192 exit inspect \u2192 reset zoom</td></tr>' +
      '</table>' +
      '<div class="fg-help-dismiss">Press Esc or click outside to close</div>' +
      '</div>';
    helpOverlay.style.display = "none";
    container.appendChild(helpOverlay);

    helpBtn.addEventListener("click", function () {
      closeExportMenu(); // don't leave two popups open at once
      helpOverlay.style.display = helpOverlay.style.display === "none" ? "flex" : "none";
    });
    helpOverlay.addEventListener("click", function (e) {
      if (e.target === helpOverlay) helpOverlay.style.display = "none";
    });

    // ── Export: turn the rendered tree into a downloadable artifact ──
    // The export reflects the CURRENT view — the active spawn-location/runtime
    // filters (workerTree/offworkerTree are rebuilt by applyFilters) — but always
    // the full (un-zoomed) trees, since an exported file should stand alone.
    let exportTitle = "dial9 flamegraph";
    // Formats a node's weight for SVG hover text. Defaults to CPU samples; the
    // heap views override it via setData so bytes/allocs render correctly.
    let exportFormatValue = null;

    // Resolve the (optional) export module once. It is statically loaded by
    // every page that uses the flamegraph, so absence means a build/wiring bug;
    // we warn once and disable the control rather than failing silently.
    const FE = getExport();
    if (!FE) {
      console.warn("flamegraph: export module (flamegraph_export.js) not loaded; export disabled");
      const wrap = searchBar.querySelector(".fg-export-wrap");
      if (wrap) wrap.style.display = "none";
    }

    function exportPanels() {
      const panels = [];
      if (workerTree) panels.push({ label: workerLabelPrefix, tree: workerTree });
      if (offworkerTree) panels.push({ label: offworkerLabelPrefix, tree: offworkerTree });
      return panels;
    }

    function hasExportableData() {
      return (workerTree && workerTree.count > 0) || (offworkerTree && offworkerTree.count > 0);
    }

    function closeExportMenu() {
      if (!exportMenu) return;
      exportMenu.style.display = "none";
      exportBtn.setAttribute("aria-expanded", "false");
    }

    // Sync the Export control to the currently-rendered trees. Called by every
    // render path (both the exact-trace applyFilters() and the aggregated/API
    // setTreeDirect()) so the button never gets stuck in its initial disabled
    // state. Always closes the menu: the trees were just rebuilt, so a menu left
    // open would refer to the previous dataset.
    function updateExportState() {
      const canExport = hasExportableData();
      exportBtn.disabled = !canExport;
      exportBtn.title = canExport ? "Export this flamegraph" : "No samples to export";
      closeExportMenu();
    }

    if (FE) {
      exportBtn.addEventListener("click", function (e) {
        e.stopPropagation();
        if (!hasExportableData()) return;
        const open = exportMenu.style.display !== "none";
        exportMenu.style.display = open ? "none" : "block";
        exportBtn.setAttribute("aria-expanded", open ? "false" : "true");
      });

      exportSvgBtn.addEventListener("click", function () {
        const svg = FE.treeToInteractiveSvg(exportPanels(), {
          title: exportTitle,
          formatValue: exportFormatValue,
        });
        downloadFile(FE.filenameStem(exportTitle) + ".svg", svg, "image/svg+xml");
        closeExportMenu();
      });

      exportFoldedBtn.addEventListener("click", function () {
        // One folded file per panel section is awkward to consume; concatenate
        // with a comment header per section so a single file round-trips. Skip
        // panels whose folded output is empty so we never emit a dangling
        // header (and so the join doesn't insert a blank section).
        const folded = exportPanels()
          .map((p) => ({ label: p.label, body: FE.treeToFolded(p.tree) }))
          .filter((s) => s.body.length > 0)
          .map((s) => "# " + s.label + "\n" + s.body)
          .join("\n");
        downloadFile(FE.filenameStem(exportTitle) + ".folded.txt", folded, "text/plain");
        closeExportMenu();
      });

      // Close the menu on any outside click. Named so destroy() can detach it.
      document.addEventListener("click", onExportOutsideClick);
    }

    function onExportOutsideClick(e) {
      if (!searchBar.contains(e.target)) closeExportMenu();
    }

    searchClear.style.display = "none";
    searchClear.addEventListener("click", function () {
      searchInput.value = "";
      searchQuery = "";
      searchClear.style.display = "none";
      hideSearchResults();
      repaint();
      searchInput.focus();
      notifyChange();
    });

    const breadcrumbBar = document.createElement("div");
    breadcrumbBar.className = "fg-breadcrumb";
    container.appendChild(breadcrumbBar);

    container.style.position = container.style.position || "relative";

    const body = document.createElement("div");
    body.className = "fg-body";
    container.appendChild(body);

    const workerLabel = document.createElement("div");
    workerLabel.className = "fg-section-label";
    workerLabel.textContent = "Worker threads";
    body.appendChild(workerLabel);

    const workerCanvas = document.createElement("canvas");
    workerCanvas.className = "fg-canvas";
    body.appendChild(workerCanvas);

    const offworkerLabel = document.createElement("div");
    offworkerLabel.className = "fg-section-label";
    offworkerLabel.textContent = "Off-worker (sampler thread)";
    body.appendChild(offworkerLabel);

    const offworkerCanvas = document.createElement("canvas");
    offworkerCanvas.className = "fg-canvas";
    body.appendChild(offworkerCanvas);

    // ── Inspect (butterfly) DOM. Hidden until inspect mode is entered. ──
    // Layout, top → bottom:
    //   [callees label]  "Callees — code called by <focus>"
    //   [callees canvas] grows UP toward the focus band (leaves at the top)
    //   [focus band]     the inspected frame, full width
    //   [callers canvas] grows DOWN (immediate callers first, then their callers)
    //   [callers label]  "Callers — paths into <focus>"
    const inspectView = document.createElement("div");
    inspectView.className = "fg-inspect";
    inspectView.style.display = "none";

    const inspectCalleesLabel = document.createElement("div");
    inspectCalleesLabel.className = "fg-section-label fg-inspect-label";
    inspectView.appendChild(inspectCalleesLabel);

    // The whole butterfly scrolls as ONE unit inside the shared .fg-body scroll
    // region (like the normal graph), rather than each half scrolling
    // separately. The focus band is position:sticky so it stays visible while
    // you scroll through tall caller/callee stacks. Callees grow up (immediate
    // callees adjacent to the band at the canvas bottom); callers grow down.
    const calleesCanvas = document.createElement("canvas");
    calleesCanvas.className = "fg-canvas";
    inspectView.appendChild(calleesCanvas);

    const focusBand = document.createElement("div");
    focusBand.className = "fg-focus-band";
    inspectView.appendChild(focusBand);

    const callersCanvas = document.createElement("canvas");
    callersCanvas.className = "fg-canvas";
    inspectView.appendChild(callersCanvas);

    const inspectCallersLabel = document.createElement("div");
    inspectCallersLabel.className = "fg-section-label fg-inspect-label";
    inspectView.appendChild(inspectCallersLabel);

    body.appendChild(inspectView);

    const tooltip = document.createElement("div");
    tooltip.className = "fg-tooltip";
    document.body.appendChild(tooltip);

    // ── Right-click context menu (Inspect / Zoom out / Copy name) ──
    const ctxMenu = document.createElement("div");
    ctxMenu.className = "fg-ctx-menu";
    ctxMenu.style.display = "none";
    ctxMenu.innerHTML =
      '<button type="button" class="fg-ctx-inspect">🔍 Inspect frame</button>' +
      '<button type="button" class="fg-ctx-zoomout">⤺ Zoom out</button>' +
      '<button type="button" class="fg-ctx-copy">⧉ Copy name</button>';
    document.body.appendChild(ctxMenu);
    const ctxInspectBtn = ctxMenu.querySelector(".fg-ctx-inspect");
    const ctxZoomOutBtn = ctxMenu.querySelector(".fg-ctx-zoomout");
    const ctxCopyBtn = ctxMenu.querySelector(".fg-ctx-copy");
    let ctxTarget = null; // { node, hitKey } captured at right-click time

    // ── Search results dropdown (issue #653) ──
    const searchResults = document.createElement("div");
    searchResults.className = "fg-search-results";
    searchResults.style.display = "none";
    searchBar.appendChild(searchResults);

    // `invert` (used by the inspect callers canvas) draws depth 0 at the TOP and
    // deeper frames downward, so the caller chains fan out below the focus band.
    // The default (invert=false) draws depth 0 at the bottom, growing up.
    function renderCanvas(canvas, data, hitKey, invert) {
      if (!data) {
        canvas.width = 0;
        canvas.height = 0;
        hitRegions[hitKey] = [];
        return;
      }
      const dpr = devicePixelRatio || 1;
      const pw = canvas.parentElement.clientWidth;
      const ph = (data.maxDepth + 2) * FG_ROW_H + 8;
      canvas.width = pw * dpr;
      canvas.height = ph * dpr;
      canvas.style.width = pw + "px";
      canvas.style.height = ph + "px";
      const ctx = canvas.getContext("2d");
      ctx.scale(dpr, dpr);
      ctx.fillStyle = "#1a1a2e";
      ctx.fillRect(0, 0, pw, ph);

      const regions = [];
      const padL = 4, padR = 4, drawW = pw - padL - padR;
      const baseY = ph - 4;
      const topPad = 4;
      ctx.font = "11px monospace";
      ctx.textBaseline = "middle";
      const qLower = searchQuery.toLowerCase();
      // In inspect mode the search box drives a results dropdown, not the
      // dim-non-matches overlay, so don't dim the butterfly by the query.
      const searching = searchQuery.length > 0 && !inspectActive;

      for (const node of data.nodes) {
        const x = padL + node.x * drawW;
        const w = node.w * drawW;
        const y = invert
          ? topPad + node.depth * FG_ROW_H
          : baseY - (node.depth + 1) * FG_ROW_H;
        if (w < 0.5) continue;

        const isAncestor = !!node.isAncestor;
        const searchMatch = !searching || node.name.toLowerCase().includes(qLower) || (node.treeNode && node.treeNode.fullName && node.treeNode.fullName.toLowerCase().includes(qLower));
        const highlighted = highlightName != null && node.name === highlightName;
        const dimmed = (searching && !searchMatch) || (highlightName != null && !highlighted);
        let alpha = 1.0;
        if (dimmed) alpha = 0.25;
        else if (isAncestor) alpha = 0.6;
        ctx.globalAlpha = alpha;
        ctx.fillStyle = flamegraphColor(node.name);
        ctx.fillRect(x, y, Math.max(w - 0.5, 0.5), FG_ROW_H - 1);
        regions.push({ x1: x, x2: x + w, y, node, totalSamples: data.totalSamples, rootTotal: data.rootTotal });

        if (w > 30) {
          ctx.fillStyle = "#fff";
          const label = node.name.length * 7 > w - 4
            ? node.name.slice(0, Math.floor((w - 10) / 7)) + "\u2026"
            : node.name;
          ctx.save();
          ctx.beginPath();
          ctx.rect(x + 2, y, w - 4, FG_ROW_H);
          ctx.clip();
          ctx.fillText(label, x + 3, y + FG_ROW_H / 2);
          ctx.restore();
        }
      }
      ctx.globalAlpha = 1.0;
      hitRegions[hitKey] = regions;
    }

    function rebuildData(key) {
      const tree = key === "worker" ? workerTree : offworkerTree;
      const stack = key === "worker" ? workerZoomStack : offworkerZoomStack;
      if (!tree) return null;
      const zoomed = stack.length > 0;
      const zoomNode = zoomed ? stack[stack.length - 1] : tree;

      // Find ancestor chain for zoomed view
      let ancestors = [];
      if (zoomed) {
        const path = findAncestorPath(tree, zoomNode);
        if (path) ancestors = path.slice(0, -1); // everything before the zoom target
      }

      const flat = flattenFromNode(zoomNode, zoomNode.count, ancestors);
      return {
        nodes: flat.nodes,
        maxDepth: flat.maxDepth,
        totalSamples: zoomNode.count,
        rootTotal: tree.count,
      };
    }

    // All source roots the butterfly/search should span — both the exact-trace
    // worker + off-worker lanes, or the single aggregated (API mode) tree.
    function sourceRoots() {
      return [workerTree, offworkerTree].filter(Boolean);
    }

    function renderAll() {
      if (inspectActive) {
        renderInspect();
        return;
      }
      workerData = rebuildData("worker");
      offworkerData = rebuildData("offworker");

      workerLabel.style.display = workerData ? "" : "none";
      workerCanvas.style.display = workerData ? "" : "none";
      offworkerLabel.style.display = offworkerData ? "" : "none";
      offworkerCanvas.style.display = offworkerData ? "" : "none";

      repaint();
      renderBreadcrumb();
    }

    function repaint() {
      if (inspectActive) {
        renderCanvas(calleesCanvas, inspectCalleesData, "callees", false);
        renderCanvas(callersCanvas, inspectCallersData, "callers", true);
        return;
      }
      renderCanvas(workerCanvas, workerData, "worker");
      renderCanvas(offworkerCanvas, offworkerData, "offworker");
      updateSearchStats();
    }

    // ── Inspect (butterfly) render/enter/exit (issue #652) ──

    // Toggle which section of the DOM (normal lanes vs. butterfly) is visible.
    function setInspectVisible(on) {
      workerLabel.style.display = on ? "none" : (workerData ? "" : "none");
      workerCanvas.style.display = on ? "none" : (workerData ? "" : "none");
      offworkerLabel.style.display = on ? "none" : (offworkerData ? "" : "none");
      offworkerCanvas.style.display = on ? "none" : (offworkerData ? "" : "none");
      inspectView.style.display = on ? "" : "none";
    }

    // Enter (or re-pivot) inspect mode focused on source-tree node `srcNode`.
    // `pushHistory` records the previous focus so the breadcrumb can walk back.
    function enterInspect(srcNode, pushHistory) {
      if (!srcNode) return;
      const res = buildInspect(sourceRoots(), srcNode);
      if (res.total <= 0) return; // frame not present (shouldn't happen)
      if (inspectActive && pushHistory !== false && inspectFocusSrc) {
        inspectHistory.push(inspectFocusSrc);
      }
      inspectActive = true;
      inspectFocusSrc = srcNode;
      inspectResult = res;
      pushInspectScroll = true; // recenter the band for this new focus
      closeContextMenu();
      // Dismiss the search box: the dropdown was just consumed, and leaving stale
      // query text would make the first Esc clear it instead of exiting inspect.
      // A fresh search while inspecting still works to pivot to another frame.
      searchInput.value = "";
      searchQuery = "";
      searchClear.style.display = "none";
      hideSearchResults();
      unpinTooltip();
      renderAll();
      renderBreadcrumb();
      // Entering/re-pivoting inspect is a view-state change, so notify the host
      // (flamegraph.html) to persist the new focus into the URL for deep links.
      notifyChange();
    }

    // Clear all inspect state without triggering a re-render. Used both by the
    // Esc/exit path and whenever the underlying trees are rebuilt (filters,
    // setTreeDirect) so the focus never dangles onto a stale tree.
    function resetInspectState() {
      inspectActive = false;
      inspectFocusSrc = null;
      inspectResult = null;
      inspectCalleesData = null;
      inspectCallersData = null;
      inspectHistory = [];
      hitRegions.callees = [];
      hitRegions.callers = [];
    }

    function exitInspect() {
      if (!inspectActive) return;
      resetInspectState();
      setInspectVisible(false);
      renderAll();
      renderBreadcrumb();
      // Leaving inspect clears the persisted focus from the URL.
      notifyChange();
    }

    function renderInspect() {
      const res = inspectResult;
      const total = res.total || 1;
      // Both trees are rooted at the focus; render their CHILDREN (depth 0+).
      const calleesFlat = flattenFromNode(res.callees, total, []);
      const callersFlat = flattenFromNode(res.callers, total, []);
      inspectCalleesData = {
        nodes: calleesFlat.nodes, maxDepth: calleesFlat.maxDepth,
        totalSamples: total, rootTotal: res.rootTotal,
      };
      inspectCallersData = {
        nodes: callersFlat.nodes, maxDepth: callersFlat.maxDepth,
        totalSamples: total, rootTotal: res.rootTotal,
      };

      setInspectVisible(true);

      const pct = res.rootTotal > 0
        ? ((res.total / res.rootTotal) * 100).toFixed(1) + "% of all samples"
        : "";
      const selfPct = res.total > 0
        ? ((res.self / res.total) * 100).toFixed(1) + "% self"
        : "";
      inspectCalleesLabel.textContent =
        "⬆ Callees — code called by " + res.focusName;
      inspectCallersLabel.textContent =
        "⬇ Callers — paths into " + res.focusName;

      // Focus band: the inspected frame, full width, with its aggregate stats.
      focusBand.innerHTML = "";
      const nameSpan = document.createElement("span");
      nameSpan.className = "fg-focus-name";
      nameSpan.textContent = res.focusName;
      nameSpan.title = res.focusFullName || res.focusName;
      focusBand.appendChild(nameSpan);
      const statSpan = document.createElement("span");
      statSpan.className = "fg-focus-stat";
      const bits = [res.total.toLocaleString() + " samples"];
      if (pct) bits.push(pct);
      if (selfPct) bits.push(selfPct);
      bits.push(res.occurrences + (res.occurrences === 1 ? " call path" : " call paths"));
      statSpan.textContent = bits.join(" · ");
      focusBand.appendChild(statSpan);
      // Fixed accent color (set in CSS) — the focus band is deliberately NOT
      // tinted by the per-frame hash color, so the pivot always looks the same.

      repaint();

      // Center the focus band in the scroll viewport on (re-)entry so both the
      // nearest callees (above) and immediate callers (below) are visible. The
      // band is sticky, so this just picks a sensible starting offset.
      if (pushInspectScroll && typeof focusBand.offsetTop === "number") {
        const bandTop = focusBand.offsetTop;
        const bandH = focusBand.offsetHeight || 0;
        const viewH = body.clientHeight || 0;
        body.scrollTop = Math.max(0, bandTop - (viewH - bandH) / 2);
      }
      pushInspectScroll = false;
    }

    function renderBreadcrumb() {
      if (inspectActive) {
        renderInspectBreadcrumb();
        return;
      }
      const wZoomed = workerZoomStack.length > 0;
      const oZoomed = offworkerZoomStack.length > 0;
      if (!wZoomed && !oZoomed) {
        breadcrumbBar.style.display = "none";
        return;
      }
      breadcrumbBar.style.display = "flex";
      breadcrumbBar.innerHTML = "";

      if (wZoomed) renderBreadcrumbFor("worker", workerZoomStack);
      if (oZoomed) {
        if (wZoomed) {
          const sep = document.createElement("span");
          sep.textContent = "  |  ";
          sep.style.color = "#555";
          breadcrumbBar.appendChild(sep);
        }
        renderBreadcrumbFor("offworker", offworkerZoomStack);
      }
    }

    function renderBreadcrumbFor(key, stack) {
      const rootSpan = document.createElement("span");
      rootSpan.className = "fg-breadcrumb-item fg-breadcrumb-link";
      rootSpan.textContent = "(all)";
      rootSpan.addEventListener("click", () => {
        if (key === "worker") workerZoomStack = [];
        else offworkerZoomStack = [];
        renderAll();
        notifyChange();
      });
      breadcrumbBar.appendChild(rootSpan);

      for (let i = 0; i < stack.length; i++) {
        const arrow = document.createElement("span");
        arrow.className = "fg-breadcrumb-sep";
        arrow.textContent = " \u203a ";
        breadcrumbBar.appendChild(arrow);

        const span = document.createElement("span");
        const isLast = i === stack.length - 1;
        span.className = "fg-breadcrumb-item" + (isLast ? "" : " fg-breadcrumb-link");
        span.textContent = stack[i].name;
        span.title = stack[i].name;
        if (!isLast) {
          const idx = i;
          span.addEventListener("click", () => {
            if (key === "worker") workerZoomStack = workerZoomStack.slice(0, idx + 1);
            else offworkerZoomStack = offworkerZoomStack.slice(0, idx + 1);
            renderAll();
            notifyChange();
          });
        }
        breadcrumbBar.appendChild(span);
      }
    }

    // Breadcrumb while inspecting: an exit link back to the flamegraph, then the
    // pivot trail (each earlier focus is clickable to jump back to it), ending at
    // the current focus.
    function renderInspectBreadcrumb() {
      breadcrumbBar.style.display = "flex";
      breadcrumbBar.innerHTML = "";

      const exitSpan = document.createElement("span");
      exitSpan.className = "fg-breadcrumb-item fg-breadcrumb-link";
      exitSpan.textContent = "⬅ Flamegraph";
      exitSpan.title = "Exit inspect (Esc)";
      exitSpan.addEventListener("click", exitInspect);
      breadcrumbBar.appendChild(exitSpan);

      const tag = document.createElement("span");
      tag.className = "fg-breadcrumb-sep";
      tag.textContent = "  ·  inspecting:  ";
      breadcrumbBar.appendChild(tag);

      const trail = inspectHistory.concat([inspectFocusSrc]);
      for (let i = 0; i < trail.length; i++) {
        if (i > 0) {
          const arrow = document.createElement("span");
          arrow.className = "fg-breadcrumb-sep";
          arrow.textContent = " › ";
          breadcrumbBar.appendChild(arrow);
        }
        const isLast = i === trail.length - 1;
        const span = document.createElement("span");
        span.className = "fg-breadcrumb-item" + (isLast ? "" : " fg-breadcrumb-link");
        span.textContent = trail[i].name;
        span.title = trail[i].fullName || trail[i].name;
        if (!isLast) {
          const idx = i;
          span.addEventListener("click", () => {
            const target = trail[idx];
            inspectHistory = trail.slice(0, idx);
            enterInspect(target, false);
          });
        }
        breadcrumbBar.appendChild(span);
      }
    }

    function updateSearchStats() {
      if (!searchQuery) {
        searchStats.textContent = "";
        return;
      }
      // Only the match COUNT here. A per-frame percentage is what's actually
      // useful, and it lives where it's unambiguous: on each dropdown row and in
      // the inspect focus band after you click a result. We deliberately do NOT
      // show an aggregate "% of samples" \u2014 for a multi-function query that's the
      // inclusive union across all matches, which reads as if it were one
      // frame's weight (e.g. a query hitting a root frame shows ~100%).
      const agg = searchAggregate(sourceRoots(), searchQuery.toLowerCase());
      if (agg.functions === 0) {
        searchStats.textContent = "no matches";
        return;
      }
      searchStats.textContent =
        agg.functions + (agg.functions === 1 ? " frame" : " frames");
    }

    searchInput.addEventListener("input", onSearchInput);
    function onSearchInput() {
      searchQuery = searchInput.value;
      searchClear.style.display = searchQuery ? "" : "none";
      renderSearchResults();
      repaint();
      // Persist the query so a shared link reproduces the same search.
      notifyChange();
    }

    // Focus the search box → re-show the results if a query is already present.
    searchInput.addEventListener("focus", function () {
      if (searchQuery) renderSearchResults();
    });

    // ── Search results dropdown (issue #653) ──
    // Max rows to render; searching a huge trace for a common substring can
    // match thousands of distinct functions and building that many DOM rows
    // janks the input. We render the biggest N and note the remainder.
    const SEARCH_RESULT_LIMIT = 200;

    function hideSearchResults() {
      searchResults.style.display = "none";
      searchResults.innerHTML = "";
    }

    // buildInspect matches occurrences by frameKey and only reads the focus
    // node's display fields, so a search-result row can pivot into inspect via a
    // lightweight synthetic focus node (no need to locate the real tree node).
    function inspectFromSearchResult(r) {
      enterInspect(
        { name: r.name, fullName: r.fullName, location: r.location, docsUrl: r.docsUrl },
        false
      );
    }

    function renderSearchResults() {
      const q = searchQuery.trim();
      if (!q) { hideSearchResults(); return; }
      const roots = sourceRoots();
      if (roots.length === 0) { hideSearchResults(); return; }
      const rootTotal = roots.reduce((n, r) => n + (r.count || 0), 0) || 1;
      const results = collectSearchResults(roots, q.toLowerCase());
      results.sort((a, b) => b.total - a.total || b.self - a.self);

      searchResults.innerHTML = "";
      if (results.length === 0) {
        const empty = document.createElement("div");
        empty.className = "fg-sr-empty";
        empty.textContent = "No matching frames";
        searchResults.appendChild(empty);
        searchResults.style.display = "block";
        return;
      }

      const header = document.createElement("div");
      header.className = "fg-sr-header";
      const shown = Math.min(results.length, SEARCH_RESULT_LIMIT);
      header.textContent =
        results.length === 1
          ? "1 matching frame — click to inspect"
          : results.length + " matching frames — click to inspect" +
            (shown < results.length ? " (top " + shown + " shown)" : "");
      searchResults.appendChild(header);

      for (let i = 0; i < shown; i++) {
        const r = results[i];
        const pct = ((r.total / rootTotal) * 100).toFixed(1);
        const row = document.createElement("div");
        row.className = "fg-sr-row";
        row.title = (r.fullName || r.name) + "\n" + r.total.toLocaleString() +
          " samples · " + r.self.toLocaleString() + " self · " +
          r.sites + (r.sites === 1 ? " call path" : " call paths");

        const bar = document.createElement("span");
        bar.className = "fg-sr-bar";
        bar.style.width = Math.max(2, Math.round(pct)) + "%";
        bar.style.background = flamegraphColor(r.name);

        const name = document.createElement("span");
        name.className = "fg-sr-name";
        name.textContent = r.name;

        const size = document.createElement("span");
        size.className = "fg-sr-size";
        size.textContent = pct + "% · " + r.total.toLocaleString() +
          (r.sites > 1 ? " · " + r.sites + " paths" : "");

        row.appendChild(bar);
        row.appendChild(name);
        row.appendChild(size);
        // Hovering a result lights up that function's frames in the graph below
        // (and dims the rest) via the shared highlight path. Match by the row's
        // display name — the same key renderCanvas highlights on.
        row.addEventListener("mouseenter", function () {
          setHighlight(r.name);
        });
        row.addEventListener("mouseleave", function () {
          if (highlightName === r.name) setHighlight(null);
        });
        row.addEventListener("click", function (e) {
          e.stopPropagation();
          inspectFromSearchResult(r);
        });
        searchResults.appendChild(row);
      }
      searchResults.style.display = "block";
    }

    function zoomTo(key, treeNode) {
      if (!treeNode || treeNode.children.size === 0) return;
      if (key === "worker") workerZoomStack.push(treeNode);
      else offworkerZoomStack.push(treeNode);
      renderAll();
      notifyChange();
    }

    function resetZoom() {
      workerZoomStack = [];
      offworkerZoomStack = [];
      renderAll();
      notifyChange();
    }

    function isZoomed() {
      return workerZoomStack.length > 0 || offworkerZoomStack.length > 0;
    }

    let tooltipPinned = false;

    function canvasKey(c) {
      if (c === workerCanvas) return "worker";
      if (c === offworkerCanvas) return "offworker";
      if (c === calleesCanvas) return "callees";
      if (c === callersCanvas) return "callers";
      return "worker";
    }

    function hitTest(e) {
      const c = e.target;
      const rect = c.getBoundingClientRect();
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      const key = canvasKey(c);
      const regions = hitRegions[key] || [];
      for (let i = regions.length - 1; i >= 0; i--) {
        const r = regions[i];
        if (mx >= r.x1 && mx <= r.x2 && my >= r.y && my < r.y + FG_ROW_H) {
          return { hit: r, hitKey: key };
        }
      }
      return { hit: null, hitKey: key };
    }

    function buildTooltipHtml(hit, pinned) {
      const node = hit.node;
      const tn = node.treeNode || {};
      const isAncestor = !!node.isAncestor;
      // Ancestors belong to the full tree; show their % relative to the root total
      const total = isAncestor ? (hit.rootTotal || hit.totalSamples || 1) : (hit.totalSamples || 1);
      const pct = ((node.count / total) * 100).toFixed(1);
      const selfPct = ((node.self / total) * 100).toFixed(1);
      const nameElt = document.createElement("b");
      nameElt.innerText = node.name;
      let h = nameElt.outerHTML;
      if (tn.fullName && tn.fullName !== node.name) {
        const fullElt = document.createElement("span");
        fullElt.innerText = tn.fullName;
        h += '<br><span style="color:#aaa">' + fullElt.innerHTML + '</span>';
      }
      if (tn.location) {
        const locShort = tn.location.replace(/^.*\/([^/]+(?::\d+)?)$/, "$1");
        const hasFullPath = tn.location !== locShort;
        if (hasFullPath) {
          const locEsc = document.createElement("span");
          locEsc.innerText = tn.location;
          h += '<br><span class="fg-loc-toggle" style="color:#888;cursor:pointer">' +
            locShort + ' <span style="color:#555">\u25b6</span></span>' +
            '<span class="fg-loc-full" style="color:#888;display:none;overflow-wrap:anywhere">' +
            locEsc.innerHTML + '</span>';
        } else {
          h += '<br><span style="color:#888">' + locShort + '</span>';
        }
      }
      h += '<br>' + (formatCount ? formatCount(node.count, total, node.self, tn) : node.count + ' samples (' + pct + '%) \u00b7 ' + node.self + ' self (' + selfPct + '%)');
      if (pinned && tn.docsUrl) {
        h += '<br><a href="' + tn.docsUrl + '" target="_blank" rel="noopener" style="color:#6c63ff;text-decoration:underline">docs.rs \u2197</a>';
      } else if (tn.docsUrl) {
        h += '<br><span style="color:#6c63ff">docs.rs \u2197</span>' +
          '<span style="color:#555"> (' + (isMac ? '\u2318' : 'Ctrl') + ' + click)</span>';
      }
      if (!pinned) {
        h += '<br><span style="color:#555">' + (isMac ? '\u2325' : 'Alt') +
          ' + click to pin \u00b7 right-click to inspect</span>';
      }
      return h;
    }

    function showTooltip(hit, x, y, pinned) {
      tooltip.innerHTML = buildTooltipHtml(hit, pinned);
      tooltip.style.pointerEvents = pinned ? "auto" : "none";
      tooltip.style.display = "block";
      // Position at top of the flamegraph container so it never covers hovered frames
      const containerRect = container.getBoundingClientRect();
      const tipX = Math.min(x + 12, window.innerWidth - tooltip.offsetWidth - 8);
      const tipY = Math.max(8, containerRect.top);
      tooltip.style.left = tipX + "px";
      tooltip.style.top = tipY + "px";
      if (pinned) {
        // Wire up expandable location
        const toggle = tooltip.querySelector(".fg-loc-toggle");
        const full = tooltip.querySelector(".fg-loc-full");
        if (toggle && full) {
          const tn = hit.node.treeNode || {};
          toggle.addEventListener("click", function () {
            if (full.style.display === "none") {
              full.textContent = tn.location;
              full.style.display = "block";
              toggle.querySelector("span").textContent = "\u25bc";
            } else {
              full.style.display = "none";
              toggle.querySelector("span").textContent = "\u25b6";
            }
          });
        }
      }
    }

    function unpinTooltip() {
      if (tooltipPinned) {
        tooltipPinned = false;
        tooltip.style.display = "none";
        tooltip.style.pointerEvents = "none";
      }
    }

    // Set the highlighted frame name (or null to clear) and repaint on the next
    // frame. Shared by canvas hover and search-result hover so both light up
    // matching frames the same way.
    function setHighlight(name) {
      if (name === highlightName) return;
      highlightName = name;
      if (!repaintQueued) {
        repaintQueued = true;
        requestAnimationFrame(() => { repaintQueued = false; repaint(); });
      }
    }

    function canvasMouseMove(e) {
      if (tooltipPinned) return;
      const { hit } = hitTest(e);
      setHighlight(hit ? hit.node.name : null);
      if (hit) {
        showTooltip(hit, e.clientX, e.clientY, false);
        e.target.style.cursor = "pointer";
      } else {
        tooltip.style.display = "none";
        e.target.style.cursor = "";
      }
    }

    function canvasMouseLeave() {
      if (!tooltipPinned) tooltip.style.display = "none";
      setHighlight(null);
    }

    function canvasClick(e, hitKey) {
      const { hit } = hitTest(e);
      if (!hit) { unpinTooltip(); return; }
      const tn = hit.node.treeNode || {};
      if ((e.metaKey || e.ctrlKey) && tn.docsUrl) {
        const w = window.open(tn.docsUrl, "_blank");
        if (w) w.focus();
      } else if (e.altKey) {
        tooltipPinned = true;
        showTooltip(hit, e.clientX, e.clientY, true);
      } else if (inspectActive) {
        // In the butterfly, a plain click on any caller/callee re-pivots the
        // inspection onto that frame (issue #652 "click to enter inspect mode").
        unpinTooltip();
        enterInspect(tn, true);
      } else {
        unpinTooltip();
        if (tn.children && tn.children.size > 0) {
          if (hit.node.isAncestor) {
            // Clicking an ancestor: zoom to that frame
            if (hitKey === "worker") workerZoomStack = [tn];
            else offworkerZoomStack = [tn];
            renderAll();
            notifyChange();
          } else {
            zoomTo(hitKey, tn);
          }
        }
      }
    }

    // Right-click opens the context menu (Inspect / Zoom out / Copy name). On a
    // frame, the menu acts on that frame; on empty space it closes any open menu.
    function canvasContextMenu(e, hitKey) {
      e.preventDefault();
      const { hit } = hitTest(e);
      if (!hit) { closeContextMenu(); return; }
      ctxTarget = { node: hit.node.treeNode || { name: hit.node.name }, hitKey: hitKey };
      openContextMenu(e.clientX, e.clientY);
    }

    function openContextMenu(x, y) {
      // "Zoom out" only makes sense in the normal zoomable view.
      ctxZoomOutBtn.style.display = inspectActive ? "none" : "";
      ctxMenu.style.display = "block";
      const mw = ctxMenu.offsetWidth, mh = ctxMenu.offsetHeight;
      ctxMenu.style.left = Math.min(x, window.innerWidth - mw - 4) + "px";
      ctxMenu.style.top = Math.min(y, window.innerHeight - mh - 4) + "px";
    }

    function closeContextMenu() {
      if (ctxMenu.style.display === "none") return;
      ctxMenu.style.display = "none";
      ctxTarget = null;
    }

    ctxInspectBtn.addEventListener("click", function (e) {
      e.stopPropagation();
      const t = ctxTarget;
      closeContextMenu();
      if (t && t.node) enterInspect(t.node, true);
    });

    ctxZoomOutBtn.addEventListener("click", function (e) {
      e.stopPropagation();
      const t = ctxTarget;
      closeContextMenu();
      if (!t || inspectActive) return;
      // Zoom out the lane you right-clicked, falling back to the other.
      const primary = t.hitKey === "offworker" ? offworkerZoomStack : workerZoomStack;
      const fallback = t.hitKey === "offworker" ? workerZoomStack : offworkerZoomStack;
      const stack = primary.length > 0 ? primary : fallback;
      if (stack.length > 0) {
        stack.pop();
        renderAll();
        notifyChange();
      }
    });

    ctxCopyBtn.addEventListener("click", function (e) {
      e.stopPropagation();
      const t = ctxTarget;
      closeContextMenu();
      if (!t || !t.node) return;
      const text = t.node.fullName || t.node.name || "";
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).catch(() => {});
      }
    });

    // Named handlers so destroy() can remove them
    function onWorkerMove(e) { canvasMouseMove(e); }
    function onOffworkerMove(e) { canvasMouseMove(e); }
    function onWorkerClick(e) { canvasClick(e, "worker"); }
    function onOffworkerClick(e) { canvasClick(e, "offworker"); }
    function onWorkerContext(e) { canvasContextMenu(e, "worker"); }
    function onOffworkerContext(e) { canvasContextMenu(e, "offworker"); }
    function onCalleesMove(e) { canvasMouseMove(e); }
    function onCallersMove(e) { canvasMouseMove(e); }
    function onCalleesClick(e) { canvasClick(e, "callees"); }
    function onCallersClick(e) { canvasClick(e, "callers"); }
    function onCalleesContext(e) { canvasContextMenu(e, "callees"); }
    function onCallersContext(e) { canvasContextMenu(e, "callers"); }

    workerCanvas.addEventListener("mousemove", onWorkerMove);
    offworkerCanvas.addEventListener("mousemove", onOffworkerMove);
    workerCanvas.addEventListener("mouseleave", canvasMouseLeave);
    offworkerCanvas.addEventListener("mouseleave", canvasMouseLeave);
    workerCanvas.addEventListener("click", onWorkerClick);
    offworkerCanvas.addEventListener("click", onOffworkerClick);
    workerCanvas.addEventListener("contextmenu", onWorkerContext);
    offworkerCanvas.addEventListener("contextmenu", onOffworkerContext);
    calleesCanvas.addEventListener("mousemove", onCalleesMove);
    callersCanvas.addEventListener("mousemove", onCallersMove);
    calleesCanvas.addEventListener("mouseleave", canvasMouseLeave);
    callersCanvas.addEventListener("mouseleave", canvasMouseLeave);
    calleesCanvas.addEventListener("click", onCalleesClick);
    callersCanvas.addEventListener("click", onCallersClick);
    calleesCanvas.addEventListener("contextmenu", onCalleesContext);
    callersCanvas.addEventListener("contextmenu", onCallersContext);

    function onKeyDown(e) {
      if (container.offsetHeight === 0) return;
      if (((e.ctrlKey || e.metaKey) && e.key === "f") || (e.key === "/" && document.activeElement !== searchInput)) {
        e.preventDefault();
        searchInput.focus();
        searchInput.select();
      }
    }
    document.addEventListener("keydown", onKeyDown);

    function isFgCanvas(t) {
      return t === workerCanvas || t === offworkerCanvas ||
             t === calleesCanvas || t === callersCanvas;
    }

    function onDocClick(e) {
      if (tooltipPinned && !tooltip.contains(e.target) && !isFgCanvas(e.target)) {
        unpinTooltip();
      }
      // Close the context menu on any click outside it (its own buttons
      // stopPropagation, so they never reach here).
      if (ctxMenu.style.display !== "none" && !ctxMenu.contains(e.target)) {
        closeContextMenu();
      }
      // Close the search-results dropdown when clicking outside the search bar.
      if (searchResults.style.display !== "none" && !searchBar.contains(e.target)) {
        hideSearchResults();
      }
    }
    document.addEventListener("click", onDocClick);

    // Returns true if consumed (search cleared or zoom reset),
    // false if nothing to do (caller should close the panel).
    function handleEscape() {
      if (ctxMenu.style.display !== "none") {
        closeContextMenu();
        return true;
      }
      if (tooltipPinned) {
        unpinTooltip();
        return true;
      }
      if (exportMenu && exportMenu.style.display !== "none") {
        closeExportMenu();
        return true;
      }
      if (helpOverlay.style.display !== "none") {
        helpOverlay.style.display = "none";
        return true;
      }
      if (searchResults.style.display !== "none") {
        hideSearchResults();
        return true;
      }
      if (searchQuery) {
        searchInput.value = "";
        searchQuery = "";
        searchClear.style.display = "none";
        renderAll();
        notifyChange();
        return true;
      }
      if (inspectActive) {
        exitInspect();
        return true;
      }
      if (isZoomed()) {
        resetZoom(); // resetZoom already calls onZoomChange
        return true;
      }
      return false;
    }

    function applyFilters() {
      const filterVal = spawnFilter.value;
      const runtimeVal = runtimeFilter.value;
      let samples = allSamples;
      if (filterVal) {
        samples = samples.filter((s) => (s.spawnLoc || "(unknown)") === filterVal);
      }
      if (runtimeVal) {
        // Off-worker samples (workerId 255) have no runtime; a runtime filter
        // excludes them. Worker samples are kept when their worker belongs to
        // the selected runtime.
        samples = samples.filter((s) => workerRuntime.get(s.workerId) === runtimeVal);
      }

      const workerSamples = samples.filter((s) => s.workerId !== 255);
      const offworkerSamples = samples.filter((s) => s.workerId === 255);

      workerTree = workerSamples.length > 0
        ? buildFlamegraphTree(workerSamples, currentSymbols)
        : null;
      offworkerTree = offworkerSamples.length > 0
        ? buildFlamegraphTree(offworkerSamples, currentSymbols)
        : null;

      workerZoomStack = [];
      offworkerZoomStack = [];
      // The focus node points into trees we just rebuilt; drop inspect state and
      // any stale search dropdown so nothing dangles onto the old trees.
      resetInspectState();
      hideSearchResults();

      workerLabel.textContent =
        `${workerLabelPrefix} \u2014 ${workerSamples.length} samples`;
      offworkerLabel.textContent =
        `${offworkerLabelPrefix} \u2014 ${offworkerSamples.length} samples`;

      updateExportState();

      renderAll();
    }

    // User-driven filter change: rebuild the trees AND persist the new filter
    // to the URL. applyFilters() itself stays notify-free so the initial load
    // (setData → applyFilters) doesn't churn the address bar.
    function onFilterChange() {
      applyFilters();
      notifyChange();
    }
    spawnFilter.addEventListener("change", onFilterChange);
    runtimeFilter.addEventListener("change", onFilterChange);

    let workerLabelPrefix = "Worker threads";
    let offworkerLabelPrefix = "Off-worker (sampler thread)";
    let formatCount = null;

    function setData(samples, callframeSymbols, opts) {
      directMode = false;
      allSamples = samples;
      currentSymbols = callframeSymbols;
      formatCount = (opts && opts.formatCount) || null;
      workerLabelPrefix = (opts && opts.workerLabel) || "Worker threads";
      offworkerLabelPrefix = (opts && opts.offworkerLabel) || "Off-worker (sampler thread)";
      exportTitle = (opts && opts.exportTitle) || "dial9 flamegraph";
      exportFormatValue = (opts && opts.exportFormatValue) || null;

      // Build spawn location dropdown
      const locCounts = new Map();
      for (const s of samples) {
        const loc = s.spawnLoc || "(unknown)";
        locCounts.set(loc, (locCounts.get(loc) || 0) + 1);
      }
      spawnFilter.innerHTML = '<option value="">All tasks (' + samples.length + ' samples)</option>';
      const sorted = [...locCounts.entries()].sort((a, b) => b[1] - a[1]);
      for (const [loc, count] of sorted) {
        const short = loc.replace(/.*\//, "");
        const opt = document.createElement("option");
        opt.value = loc;
        opt.textContent = short + " (" + count + ")";
        opt.title = loc;
        spawnFilter.appendChild(opt);
      }

      buildRuntimeFilter(samples, opts && opts.runtimeWorkers);

      applyFilters();
    }

    // Build the runtime-filter dropdown from the trace's runtime.<name>
    // segment metadata. Only shown when the trace actually has more than one
    // runtime; otherwise the control stays hidden (single-runtime traces look
    // exactly as before). Mirrors the multi-runtime lane grouping in the
    // timeline view so the two stay consistent.
    function buildRuntimeFilter(samples, runtimeWorkers) {
      runtimeFilter.style.display = "none";
      runtimeFilter.value = "";
      const data = buildRuntimeFilterData
        ? buildRuntimeFilterData(samples, runtimeWorkers)
        : { workerRuntime: new Map(), options: [] };
      workerRuntime = data.workerRuntime;
      if (data.options.length === 0) return; // single runtime: nothing to filter

      runtimeFilter.innerHTML = '<option value="">All runtimes</option>';
      for (const o of data.options) {
        const opt = document.createElement("option");
        opt.value = o.name;
        const label = o.inferred ? `${o.name} runtime` : `runtime: ${o.name}`;
        opt.textContent = `${label} (${o.sampleCount})`;
        opt.title = label;
        runtimeFilter.appendChild(opt);
      }
      runtimeFilter.style.display = "";
    }

    function resize() {
      if (inspectActive) {
        renderCanvas(calleesCanvas, inspectCalleesData, "callees", false);
        renderCanvas(callersCanvas, inspectCallersData, "callers", true);
        return;
      }
      renderCanvas(workerCanvas, workerData, "worker");
      renderCanvas(offworkerCanvas, offworkerData, "offworker");
    }

    function destroy() {
      document.removeEventListener("keydown", onKeyDown);
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("click", onExportOutsideClick);
      workerCanvas.removeEventListener("mousemove", onWorkerMove);
      offworkerCanvas.removeEventListener("mousemove", onOffworkerMove);
      workerCanvas.removeEventListener("mouseleave", canvasMouseLeave);
      offworkerCanvas.removeEventListener("mouseleave", canvasMouseLeave);
      workerCanvas.removeEventListener("click", onWorkerClick);
      offworkerCanvas.removeEventListener("click", onOffworkerClick);
      workerCanvas.removeEventListener("contextmenu", onWorkerContext);
      offworkerCanvas.removeEventListener("contextmenu", onOffworkerContext);
      calleesCanvas.removeEventListener("mousemove", onCalleesMove);
      callersCanvas.removeEventListener("mousemove", onCallersMove);
      calleesCanvas.removeEventListener("mouseleave", canvasMouseLeave);
      callersCanvas.removeEventListener("mouseleave", canvasMouseLeave);
      calleesCanvas.removeEventListener("click", onCalleesClick);
      callersCanvas.removeEventListener("click", onCallersClick);
      calleesCanvas.removeEventListener("contextmenu", onCalleesContext);
      callersCanvas.removeEventListener("contextmenu", onCallersContext);
      searchInput.removeEventListener("input", onSearchInput);
      if (tooltip.parentNode) tooltip.parentNode.removeChild(tooltip);
      if (ctxMenu.parentNode) ctxMenu.parentNode.removeChild(ctxMenu);
      container.innerHTML = "";
    }

    function getZoomPath() {
      function fullPath(tree, stack) {
        if (!tree || stack.length === 0) return [];
        // If stack already has the full path (from zoomToPath restore), use it directly.
        // Otherwise find the path from root to the zoom target.
        const target = stack[stack.length - 1];
        const path = findNodePath(tree, target.name);
        return path ? path.map((n) => n.name) : stack.map((n) => n.name);
      }
      return {
        worker: fullPath(workerTree, workerZoomStack),
        offworker: fullPath(offworkerTree, offworkerZoomStack),
      };
    }

    // Find a node by name anywhere in the tree via DFS, return path from root.
    function findNodePath(tree, name) {
      const path = [];
      function dfs(node) {
        path.push(node);
        if (node.name === name) return true;
        for (const child of node.children.values()) {
          if (dfs(child)) return true;
        }
        path.pop();
        return false;
      }
      for (const child of tree.children.values()) {
        if (dfs(child)) return path;
      }
      return null;
    }

    // Like findNodePath but uses object identity instead of name matching.
    function findAncestorPath(tree, target) {
      const path = [];
      function dfs(node) {
        path.push(node);
        if (node === target) return true;
        for (const child of node.children.values()) {
          if (dfs(child)) return true;
        }
        path.pop();
        return false;
      }
      for (const child of tree.children.values()) {
        if (dfs(child)) return path;
      }
      return null;
    }

    function zoomToPath(key, names) {
      const tree = key === "worker" ? workerTree : offworkerTree;
      if (!tree || !names.length) return;
      const stack = key === "worker" ? workerZoomStack : offworkerZoomStack;
      // Try walking child-by-child (works when names is a full path from root)
      let node = tree;
      for (let i = 0; i < names.length; i++) {
        let found = null;
        for (const child of node.children.values()) {
          if (child.name === names[i]) { found = child; break; }
        }
        if (!found) {
          // Path walk failed, fall back to DFS for the last name
          const path = findNodePath(tree, names[names.length - 1]);
          if (path) stack.push.apply(stack, path);
          break;
        }
        stack.push(found);
        node = found;
      }
      if (stack.length > 0) renderAll();
    }

    // Deep-link support for the inspect (butterfly) focus. The focus is
    // identified by its frameKey (fullName || name) so it survives tree
    // rebuilds and streamed refinements, and can be reconstructed from a URL.
    function getInspectFocus() {
      return inspectActive && inspectFocusSrc ? frameKey(inspectFocusSrc) : null;
    }

    // Find a source-tree node whose frameKey matches `key`, anywhere in the tree.
    function findNodeByKey(tree, key) {
      let found = null;
      function dfs(node) {
        if (frameKey(node) === key) { found = node; return true; }
        for (const child of node.children.values()) {
          if (dfs(child)) return true;
        }
        return false;
      }
      for (const child of tree.children.values()) {
        if (dfs(child)) break;
      }
      return found;
    }

    // Restore inspect mode focused on the frame identified by `key`. No-op if
    // the frame is not present in the current trees (e.g. filtered out).
    function focusInspectByKey(key) {
      if (!key) return false;
      for (const root of sourceRoots()) {
        const node = findNodeByKey(root, key);
        if (node) { enterInspect(node, false); return true; }
      }
      return false;
    }

    // The current search query (the frames-search box text), or "" when empty.
    function getSearch() {
      return searchQuery;
    }

    // Programmatically set the search query (used by view-state restore). Mirrors
    // the input handler minus the notify — restore drives this under suspend.
    function setSearch(q) {
      q = q || "";
      searchInput.value = q;
      searchQuery = q;
      searchClear.style.display = q ? "" : "none";
      renderSearchResults();
      repaint();
    }

    // The active spawn-location / runtime filter values ("" = no filter). Empty
    // in aggregated/API mode, where these controls are hidden and inapplicable.
    function getSpawnFilter() {
      return directMode ? "" : (spawnFilter.value || "");
    }
    function getRuntimeFilter() {
      return directMode ? "" : (runtimeFilter.value || "");
    }

    // Set a filter's value only if that exact option exists (a stale link into a
    // trace lacking the option is ignored rather than selecting an empty value).
    function setSelectIfPresent(sel, value) {
      if (!value) { sel.value = ""; return; }
      for (const opt of sel.options) {
        if (opt.value === value) { sel.value = value; return; }
      }
    }

    // The complete, serializable view state — the exact shape the URL codec in
    // flamegraph_view_state.js reads/writes. Absent pieces are simply omitted so
    // the codec deletes their keys. `inspect` carries the name/symbol split so a
    // restored link re-derives the same identity key (fullName || name).
    function getViewState() {
      const z = getZoomPath();
      const out = {};
      if (z.worker && z.worker.length) out.workerZoom = z.worker;
      if (z.offworker && z.offworker.length) out.offworkerZoom = z.offworker;
      if (inspectActive && inspectFocusSrc) {
        out.inspect = {
          name: inspectFocusSrc.name,
          fullName: inspectFocusSrc.fullName || inspectFocusSrc.name,
        };
      }
      if (searchQuery) out.search = searchQuery;
      const spawn = getSpawnFilter();
      if (spawn) out.spawn = spawn;
      const runtime = getRuntimeFilter();
      if (runtime) out.runtime = runtime;
      return out;
    }

    // Restore a view state produced by getViewState (typically decoded from the
    // URL). Silent by default: mutating zoom/inspect/search/filters here must not
    // fire the persist callback (that would rewrite the URL mid-restore). Order
    // matters: filters rebuild the trees and reset zoom, so they run FIRST, then
    // the zoom path, then inspect, then search.
    function applyViewState(state, opts) {
      state = state || {};
      const silent = !opts || opts.silent !== false;
      const prev = suspendNotify;
      if (silent) suspendNotify = true;
      try {
        // Full restore (not a merge): drive every dimension to the state's value,
        // including "absent" → cleared, so re-applying the same URL over several
        // streamed snapshots converges instead of accumulating.
        resetView();
        // Filters only apply in exact mode (raw samples present). Applying them
        // rebuilds the trees + resets zoom, so they must precede zoom/inspect.
        // resetView above does not touch the filter <select>s, so set them here
        // (to the target, or "" to clear) and rebuild — but only when the value
        // actually changes, to avoid a redundant full tree rebuild on the common
        // fresh-load case where no filter is being restored.
        if (!directMode) {
          const wantSpawn = state.spawn || "";
          const wantRuntime = state.runtime || "";
          if (spawnFilter.value !== wantSpawn || runtimeFilter.value !== wantRuntime) {
            setSelectIfPresent(spawnFilter, wantSpawn);
            setSelectIfPresent(runtimeFilter, wantRuntime);
            applyFilters();
          }
        }
        if (state.workerZoom && state.workerZoom.length) {
          zoomToPath("worker", state.workerZoom);
        }
        if (state.offworkerZoom && state.offworkerZoom.length) {
          zoomToPath("offworker", state.offworkerZoom);
        }
        if (state.inspect) {
          // The identity key is fullName || name — the same key focusInspectByKey
          // matches on and getInspectFocus reports.
          focusInspectByKey(state.inspect.fullName || state.inspect.name);
        }
        setSearch(state.search || "");
      } finally {
        suspendNotify = prev;
      }
    }

    // Clear zoom + inspect WITHOUT notifying the host. Used by flamegraph.html's
    // URL-restore retries (the aggregate tree streams in, so restore may run over
    // several snapshots): each attempt resets first, making re-applying a URL
    // zoom path idempotent (zoomToPath appends, so it must start from a clean
    // stack). Not part of the user-facing zoom-out flow — that's resetZoom(),
    // which does notify.
    function resetView() {
      workerZoomStack = [];
      offworkerZoomStack = [];
      if (inspectActive) {
        resetInspectState();
        setInspectVisible(false);
      }
      renderAll();
    }

    function setTreeDirect(tree, totalCount) {
      directMode = true;
      // For API mode: set a pre-built tree directly (no worker/off-worker split)
      // Preserve the current zoom by finding the same node in the new tree.
      const prevTarget = workerZoomStack.length > 0
          ? workerZoomStack[workerZoomStack.length - 1].name
          : null;
      workerTree = tree;
      offworkerTree = null;
      offworkerZoomStack = [];
      // Re-resolve: find the zoom target by name in the new tree via DFS.
      workerZoomStack = [];
      if (prevTarget) {
        const path = findNodePath(workerTree, prevTarget);
        if (path) workerZoomStack = path;
      }
      // Aggregated trees are not split into worker/off-worker lanes, so the
      // exported section header should read "All threads" to match the label
      // shown on screen (rather than the default "Worker threads" prefix).
      workerLabelPrefix = "All threads";
      workerLabel.textContent = `All threads \u2014 ${totalCount.toLocaleString()} samples`;
      offworkerLabel.textContent = "";
      offworkerCanvas.style.display = "none";
      offworkerLabel.style.display = "none";
      spawnFilter.style.display = "none";
      runtimeFilter.style.display = "none";
      // Enable the Export control now that an aggregated tree is rendered \u2014 the
      // exact-trace path does this in applyFilters(), but API mode bypasses it.
      updateExportState();
      hideSearchResults();
      // API mode streams refinements: if the user is inspecting a frame, keep
      // the butterfly live by re-computing it against the freshly-set tree
      // (buildInspect only reads the focus's identity, so the old focus node is
      // still a valid seed). Otherwise fall back to the normal render.
      if (inspectActive && inspectFocusSrc) {
        const res = buildInspect(sourceRoots(), inspectFocusSrc);
        if (res.total > 0) {
          inspectResult = res;
          renderAll();
          renderBreadcrumb();
          return;
        }
        // Focus vanished from the new tree \u2014 drop back to the flamegraph.
        resetInspectState();
        setInspectVisible(false);
      }
      renderAll();
    }

    return {
      setData, setTreeDirect, resize, destroy, handleEscape, isZoomed,
      getZoomPath, zoomToPath, getInspectFocus, focusInspectByKey, resetView,
      // Consolidated view-state accessors (shape matches flamegraph_view_state.js).
      getViewState, applyViewState,
      getSearch, setSearch, getSpawnFilter, getRuntimeFilter,
    };
  }

  const fgExports = {
    createFlamegraph: createFlamegraph,
    filterCpuSamples: filterCpuSamples,
    // Pure helpers exported for tests (issues #652/#653).
    buildInspect: buildInspect,
    collectSearchResults: collectSearchResults,
    searchAggregate: searchAggregate,
  };
  if (typeof module !== "undefined" && module.exports) {
    module.exports = fgExports;
  } else {
    exports.FlamegraphRenderer = fgExports;
  }
})(typeof exports === "undefined" ? this : exports);
