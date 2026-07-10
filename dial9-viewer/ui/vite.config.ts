/// <reference types="vitest/config" />
import { defineConfig, type Plugin } from "vite";
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
// (url_state.js, flamegraph_api.js, ui-switch.js). Derived from the pages'
// actual tags; re-derive with:
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
  // The dual-UI switch (T38, ADR-0004 section 8): loaded by all four legacy
  // pages AND by future new-UI entries; plain browser JS, copied not bundled.
  "ui-switch.js",
  "url_state.js",
];

// Root-served verbatim assets (demo-trace.bin, flamegraph.css) live in
// public/: Vite copies public/ into the dist root unchanged (build) and
// serves it at / (dev), so the pages' root-relative references keep working
// in both modes with no static-copy entry.

// Single-server dev loop (`npm run dev:embedded` = `vite build --watch`,
// 02-architecture.md section 3): Rollup only rebuilds when files in its
// module graph change, and the statically-copied legacy files above are NOT
// in that graph (vite-plugin-static-copy re-copies per build but watches
// nothing in build mode). Register them as watch files so editing a legacy
// page or script triggers a rebuild - whose writeBundle re-runs the static
// copy into dist/ - preserving the edit-refresh loop (constraint H5).
function watchLegacyFiles(files: string[]): Plugin {
  return {
    name: "dial9:watch-legacy-static-files",
    apply: "build",
    buildStart() {
      for (const f of files) {
        // Relative paths resolve against process.cwd() (= ui/ under npm
        // scripts), matching the static-copy `src` entries above.
        this.addWatchFile(f);
      }
    },
  };
}

export default defineConfig({
  // Vitest, the single test runner (ADR-0004 section 7, 02-architecture.md
  // N13). Config lives here, not in a separate vitest.config file
  // (02-architecture.md section 2.1). Migrated legacy suites live under
  // tests/core/ (core-suite colocation); suites for non-core ui/-root scripts
  // (e.g. ui-switch.js) live directly under tests/; new TS modules colocate
  // their tests under src/. The legacy `node test_*.js` runner is retired
  // (T11); the one remaining root script, test_parser.js, is driven by the
  // Rust integration test tests/js_parser.rs, not by Vitest.
  test: {
    environment: "node",
    include: ["tests/**/*.test.ts", "src/**/*.test.ts"],
    // Many suites parse the 3.4MB demo trace in beforeAll/tests; with the
    // whole tests/core/ set running in parallel workers (and on slow CI
    // runners), individual parses far exceed Vitest's 5s/10s defaults. These
    // are ceilings for stragglers, not expected durations.
    testTimeout: 120_000,
    hookTimeout: 120_000,
  },
  // Proxy-mode dev loop (`npm run dev`): Vite serves the UI with HMR and
  // forwards /api/* to the Rust dev-server, launched with:
  //   PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server
  server: {
    proxy: {
      "/api": "http://localhost:3001",
    },
  },
  build: {
    target: "es2022",
    sourcemap: false,
    // T16: rollup's CJS interop only covers node_modules by default, but
    // the frozen core is CJS-guard .js at the ui ROOT (ADR-0004 section
    // 6), consumed via named ESM imports from lib/trace + lib/canvas.
    // vite-node (dev/test) interops via Node require semantics; the
    // BUILD needs the commonjs plugin to cover the root modules too, or
    // any rollup graph that reaches the core ("is not exported by
    // trace_parser.js") fails - first exercised by the worker entry.
    // "*.js" resolves against the ui/ cwd: root-level .js only. decode.js
    // is a SYMLINK into dial9-trace-format/js/ and Vite ids modules by
    // realpath, so its target directory needs its own pattern.
    commonjsOptions: {
      include: [/node_modules/, "*.js", "../../dial9-trace-format/js/*.js"],
      // Keep the core's Node-only dynamic require (trace_parser.js
      // getTraceDecoder: `require(path.resolve(__dirname, "decode.js"))`)
      // as a literal `require` in the output instead of rewriting it to a
      // throwing helper. In a bundled module worker `typeof require` is
      // then genuinely "undefined", so the core takes its browser branch
      // (the TraceDecoder global, seeded by the worker entry) - the same
      // resolution the legacy <script src> pages use.
      ignoreDynamicRequires: true,
    },
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
      ],
    }),
    // public/ is re-copied by each rebuild too, so watching its two assets
    // keeps the dev:embedded loop alive for them as well.
    watchLegacyFiles([
      ...legacyPages,
      ...legacyPageScripts,
      "public/flamegraph.css",
      "public/demo-trace.bin",
    ]),
  ],
});
