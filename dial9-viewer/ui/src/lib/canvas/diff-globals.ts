// Seeds the bare globals that the frozen flamegraph_diff_view.js reads at
// evaluation time: the `FlamegraphDiff` namespace (from flamegraph_diff.js) and
// `formatHumanDuration`. It MUST be a SEPARATE module, imported for side effect
// before flamegraph_diff_view.js is re-exported: an `export ... from` re-export
// evaluates its target during the linking phase, before the importing module's
// own body runs, so an inline seed would land too late. Mirrors the widget's
// export-globals.js / core-globals.js seed chain.

import * as FlamegraphDiff from "../../../flamegraph_diff.js";
import { formatHumanDuration } from "../../../format.js";

const g = globalThis as Record<string, unknown>;
g["FlamegraphDiff"] ??= FlamegraphDiff;
g["formatHumanDuration"] ??= formatHumanDuration;
