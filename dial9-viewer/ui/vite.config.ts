/// <reference types="vitest/config" />
import { defineConfig, type Plugin } from "vite";
import { viteStaticCopy } from "vite-plugin-static-copy";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import path from "node:path";

// Dev-mode CJS interop for the frozen core (ADR-0004 section 6).
//
// The frozen-core files (decode.js, trace_parser.js, ...) are CommonJS-guarded
// browser scripts at the ui root. They declare exports in mixed styles
// (`exports.X = ...` through an IIFE parameter, `module.exports = {...}` object
// literals, ...) which Vite's dev browser transform cannot statically see, so
// it exposes only a `default` export. The lib/trace + lib/canvas barrels import
// the core by NAME (`import { makeTimePanelLayout } from ".../panel_layout.js"`)
// and so throw at boot in `npm run dev`: "does not provide an export named ...".
// Every new page breaks (they all reach trace_parser/decode/... this way).
//
// Two things stack against a naive fix: (1) the dev export-detector
// (cjs-module-lexer) misses the guarded names; (2) these files are ALSO in the
// static-copy list (the legacy pages load them as classic `<script src>`), and
// in dev vite-plugin-static-copy serves them RAW over HTTP for ANY request to
// `/panel_layout.js` (query included), which overrides the module pipeline.
//
// So this plugin diverts only the ESM IMPORTS - never the classic-script
// requests, which don't pass through resolveId - to a `\0` virtual module that
// static-copy cannot match. The virtual module executes the frozen-core CJS
// inline (self-contained: no re-import of the static URL) and re-exports the
// names, which are discovered by requiring each core file in Node here - the
// same CJS the build and tests run, robust across the files' export styles.
//
// Scope: `serve` only, skipped under VITEST. Build is unaffected
// (build.commonjsOptions owns it); Vitest is unaffected (vite-node Node
// require). The frozen-core files on disk are never touched.
function frozenCoreDevInterop(): Plugin {
  const require = createRequire(import.meta.url);
  const CORE = [
    "decode",
    "trace_parser",
    "trace_analysis",
    "format",
    "heatmap",
    "prefix_detect",
    "creds",
    "panel_layout",
    "flamegraph",
    "flamegraph_export",
    "flamegraph_api",
    // Post-migration modules the merge brought in; each is named-imported by a
    // lib seam, so dev (npm run dev) needs the ESM-named-export shim too.
    "flamegraph_diff",
    "flamegraph_diff_view",
    "flamegraph_histogram",
    "sse",
    "tokio_stats_api",
    "trace_scope",
    "span_explorer",
  ];
  const VIRT = "\0d9core:";
  const uiRoot = process.cwd(); // npm scripts run from ui/
  const exportNames = new Map<string, string[]>();
  for (const base of CORE) {
    try {
      const mod = require(`${uiRoot}/${base}.js`) as Record<string, unknown>;
      exportNames.set(
        base,
        Object.keys(mod).filter(
          (k) => k !== "default" && /^[A-Za-z_$][\w$]*$/.test(k),
        ),
      );
    } catch {
      // Leave empty: the shim still passes `default` through, so a core file
      // that fails to require in Node degrades to default-only rather than
      // breaking the whole dev server.
      exportNames.set(base, []);
    }
  }
  return {
    name: "dial9:frozen-core-dev-interop",
    apply: (_config, env) => env.command === "serve" && !process.env.VITEST,
    enforce: "pre",
    async resolveId(source, importer, options) {
      if (source.startsWith(VIRT)) return source; // already ours
      if (importer === undefined) return null;
      const base = /(?:^|\/)([A-Za-z_]+)\.js$/.exec(source)?.[1];
      if (base === undefined || !CORE.includes(base)) return null;
      // Confirm it resolves to the ROOT core file, not a src TS module.
      const resolved = await this.resolve(source, importer, {
        ...options,
        skipSelf: true,
      });
      if (resolved === null) return null;
      const clean = resolved.id.split("?")[0] ?? resolved.id;
      if (clean.includes("/src/") || !clean.endsWith(`/${base}.js`)) return null;
      return `${VIRT}${clean}`;
    },
    load(id) {
      if (!id.startsWith(VIRT)) return null;
      const file = id.slice(VIRT.length);
      const base = path.basename(file, ".js");
      const src = readFileSync(file, "utf8");
      const names = exportNames.get(base) ?? [];
      // Run the CJS body with a local module/exports so its `typeof module`
      // guard populates our object; call with globalThis as `this` to match
      // classic-script semantics for any UMD header that reads `this`.
      return [
        `const __d9mod = { exports: {} };`,
        `(function (module, exports) {`,
        src,
        `}).call(globalThis, __d9mod, __d9mod.exports);`,
        `export default __d9mod.exports;`,
        ...names.map(
          (n) => `export const ${n} = __d9mod.exports[${JSON.stringify(n)}];`,
        ),
      ].join("\n");
    },
  };
}

// Scaffolding config (docs/ui-inventory/02-architecture.md section 2.1,
// docs/adr/0004-viewer-ui-migration.md section 3).
//
// No page has migrated yet: the four legacy HTML pages and every file they
// reference via <script src> ship into dist/ VERBATIM through the static-copy
// list below, so the served pages stay byte-identical to serving ui/ from
// disk. As pages convert to real Vite entries (the viewer
// slices), their lines are removed from this list until it is empty.
//
// The frozen core files (decode.js, trace_parser.js, ... - see AGENTS.md)
// stay at the ui/ root in their browser-global + CommonJS-guard form; they
// are copied, never bundled/minified, while legacy pages load them via
// <script src>.
//
// The `ui/` package deliberately has NO `"type": "module"` in package.json:
// `node test_*.js` must keep treating the root scripts as CommonJS.

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
  "flamegraph_diff.js",
  "flamegraph_diff_view.js",
  "flamegraph_export.js",
  "flamegraph_histogram.js",
  "flamegraph_view_state.js",
  "format.js",
  "heatmap.js",
  "panel_layout.js",
  "prefix_detect.js",
  "sse.js",
  "tokio_stats_api.js",
  "trace_analysis.js",
  "trace_parser.js",
  "trace_scope.js",
  // The dual-UI switch (ADR-0004 section 8): loaded by all four legacy
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
// copy into dist/ - preserving the edit-refresh loop.
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
  // Vitest, the single test runner (ADR-0004 section 7, 02-architecture.md).
  // Config lives here, not in a separate vitest.config file
  // (02-architecture.md section 2.1). Migrated legacy suites live under
  // tests/core/ (core-suite colocation); suites for non-core ui/-root scripts
  // (e.g. ui-switch.js) live directly under tests/; new TS modules colocate
  // their tests under src/. The legacy `node test_*.js` runner is retired;
  // the one remaining root script, test_parser.js, is driven by the
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
    // Rollup's CJS interop only covers node_modules by default, but
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
      // Real page entries (each page ships ONE bundle, so no shared
      // placeholder inputs: the dev-probe module was retired when
      // the first real entry landed, as its header said it would be).
      input: {
        // The migrated flamegraph page, served at /new/flamegraph.html
        // (HTML inputs keep their project-root-relative path in dist/).
        flamegraph: "new/flamegraph.html",
        // The migrated S3 browser page, served off-root at
        // new/index.html (registered in ui-switch.js NEW_UI_ENTRIES; the
        // canonical /index.html keeps serving the legacy page).
        "new-index": "new/index.html",
        // The migrated trace-viewer shell, served off-root at
        // new/viewer.html (registered in ui-switch.js NEW_UI_ENTRIES; the
        // canonical /viewer.html keeps serving the legacy page).
        "new-viewer": "new/viewer.html",
        // The migrated Tokio Stats page, served off-root at
        // new/tokio_stats.html (registered in ui-switch.js NEW_UI_ENTRIES;
        // the canonical /tokio_stats.html keeps serving the legacy page).
        "new-tokio-stats": "new/tokio_stats.html",
        // The Span Explorer. Born after the migration, so it has NO legacy
        // twin and is served at its CANONICAL root path rather than under
        // new/: there is nothing for ui-switch.js to switch between.
        "span-explorer": "span_explorer.html",
      },
    },
  },
  plugins: [
    // Dev-only: give the frozen-core CJS files real ESM named exports so the
    // lib/trace + lib/canvas barrels resolve in `npm run dev` (see the
    // frozenCoreDevInterop docs above). No-op for build / vitest.
    frozenCoreDevInterop(),
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
