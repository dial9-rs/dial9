import { defineConfig } from "vite";
import { viteStaticCopy } from "vite-plugin-static-copy";

// T02 scaffolding config (docs/ui-inventory/02-architecture.md section 2.1,
// docs/adr/0004-viewer-ui-migration.md section 3).
//
// No page has migrated yet: the four legacy HTML pages and every file they
// reference via <script src> ship into dist/ VERBATIM through the static-copy
// list below, so the served pages stay byte-identical to serving ui/ from
// disk. As pages convert to real Vite entries (T13/T14/T41 and the viewer
// slices), their lines are removed from this list until it is empty.
//
// The frozen core files (decode.js, trace_parser.js, ... - see AGENTS.md)
// stay at the ui/ root in their browser-global + CommonJS-guard form; they
// are copied, never bundled/minified, while legacy pages load them via
// <script src> (constraint H2).
//
// The `ui/` package deliberately has NO `"type": "module"` in package.json:
// `node test_*.js` must keep treating the root scripts as CommonJS (H2).

// Legacy pages still served as-is (tokio_stats.html included: it has no
// Vite-entry ticket in chunk 1 but must keep being served).
const legacyPages = [
  "index.html",
  "viewer.html",
  "flamegraph.html",
  "tokio_stats.html",
];

// Everything the four legacy pages reference by root-relative path
// (<script src> / <link>): the frozen core plus the post-freeze page modules
// (url_state.js, flamegraph_api.js). Derived from the pages' actual tags;
// re-derive with:
//   grep -h 'script src=\|stylesheet' ui/{index,viewer,flamegraph,tokio_stats}.html | sort -u
const legacyPageScripts = [
  "creds.js",
  "decode.js",
  "flamegraph.js",
  "flamegraph_api.js",
  "flamegraph_export.js",
  "format.js",
  "heatmap.js",
  "panel_layout.js",
  "prefix_detect.js",
  "trace_analysis.js",
  "trace_parser.js",
  "url_state.js",
];

// Root-served assets that STAY at ui/ root (orchestrator ruling 2026-07-08,
// recorded in docs/tickets/chunk-1-foundation.md T02): moving them to
// public/ before T04 would break the disk-served dev loop (H5) and orphan
// ~18 disk readers of demo-trace.bin (test_*.js, Rust tests/benches, the
// regeneration pipeline, stress CI). T04 owns the public/ move.
const legacyPageAssets = ["flamegraph.css", "demo-trace.bin"];

export default defineConfig({
  build: {
    target: "es2022",
    sourcemap: false,
    rollupOptions: {
      // No migrated page entries exist yet; this placeholder module gives
      // `vite build` an input (and gives T04 an HMR probe target). Page
      // tickets replace it with real HTML entries.
      input: {
        "dev-probe": "src/pages/dev-probe.ts",
      },
    },
  },
  plugins: [
    viteStaticCopy({
      targets: [
        ...legacyPages.map((f) => ({ src: f, dest: "." })),
        ...legacyPageScripts.map((f) => ({ src: f, dest: "." })),
        ...legacyPageAssets.map((f) => ({ src: f, dest: "." })),
      ],
    }),
  ],
});
