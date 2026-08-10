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
// Vite's dev export-detector (cjs-module-lexer) misses the guarded names. In
// addition, creds.js is served raw for its stable browser-global contract, so
// its module import must not be intercepted by the static-copy plugin.
//
// This plugin diverts only ESM imports to a `\0` virtual module. The virtual
// module executes the frozen-core CJS inline and re-exports the names discovered
// by requiring each core file in Node.
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
    "session",
    "panel_layout",
    "flamegraph",
    "flamegraph_export",
    "flamegraph_api",
    // Shared modules named-imported by a lib seam.
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

// The canonical pages are Vite entries. Only the two browser-global contracts
// loaded by index.html remain verbatim assets; shared core modules are bundled
// through the typed src/lib seams.
const classicScripts = [
  "creds.js",
  "url_state.js",
];

// Root-served verbatim assets (demo-trace.bin, flamegraph.css) live in
// public/: Vite copies public/ into the dist root unchanged (build) and
// serves it at / (dev), so the pages' root-relative references keep working
// in both modes with no static-copy entry.

// Rollup does not watch statically copied files in build mode. Register the
// classic scripts and public assets so `npm run dev:embedded` stays live.
function watchStaticFiles(files: string[]): Plugin {
  return {
    name: "dial9:watch-static-files",
    apply: "build",
    buildStart() {
      for (const f of files) {
        this.addWatchFile(f);
      }
    },
  };
}

export default defineConfig({
  // Vitest, the single test runner (ADR-0004 section 7, 02-architecture.md).
  // Config lives here, not in a separate vitest.config file
  // (02-architecture.md section 2.1). Frozen-core suites live under
  // tests/core/; new TS modules colocate their tests under src/. The legacy
  // `node test_*.js` runner is retired;
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
    // Do NOT wipe dist/ on build. `dist/.gitkeep` is COMMITTED (ui/.gitignore
    // ignores `dist/*` but re-includes it) because rust-embed embeds `ui/dist/`
    // and needs the folder to exist for a cargo-only checkout to compile — see
    // dial9-viewer/src/server/mod.rs. Vite's default emptyOutDir deleted it, so
    // any `npm run build` left a staged deletion that silently broke that build
    // for whoever committed afterwards.
    //
    // Trade-off, stated honestly: content-hashed assets DO accumulate. A no-op
    // rebuild reuses the same hashes, but any content change emits a new
    // `assets/<name>-<hash>.js` and the previous one is never reclaimed, so a
    // long-lived working tree grows one orphan per change. That is local-only
    // clutter: release CI and the binary workflows both `npm ci && npm run
    // build` in a FRESH checkout where dist/ holds just `.gitkeep`, so no stale
    // asset can be embedded into a published crate or binary, and .gitignore
    // keeps orphans out of git. `npx vite build --emptyOutDir` cleans up a tree
    // that has drifted.
    //
    // The cleaner fix is to gitignore dist/ entirely and create it from
    // dial9-viewer/build.rs (which already runs and declares
    // rerun-if-changed=ui), which would let Vite keep its default. Deferred:
    // that moves a Rust build dependency and wants verifying against a genuine
    // cargo-only checkout, which is more than this fix needs to be.
    emptyOutDir: false,
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
      // Canonical multi-page entries.
      input: {
        index: "index.html",
        viewer: "viewer.html",
        flamegraph: "flamegraph.html",
        "tokio-stats": "tokio_stats.html",
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
      targets: classicScripts.map((f) => ({ src: f, dest: "." })),
    }),
    // public/ is re-copied by each rebuild too, so watching its two assets
    // keeps the dev:embedded loop alive for them as well.
    watchStaticFiles([
      ...classicScripts,
      "public/flamegraph.css",
      "public/demo-trace.bin",
    ]),
  ],
});
