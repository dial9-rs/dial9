// Seeds the bare globals that the frozen flamegraph_diff_view.js reads at
// evaluation time: the `FlamegraphDiff` and `FlamegraphApi` namespaces and
// `formatHumanDuration`. Those modules self-publish onto the window only as
// classic <script> tags. It MUST be a SEPARATE module, imported for side effect
// before flamegraph_diff_view.js is re-exported: an `export ... from` re-export
// evaluates its target during the linking phase, before the importing module's
// own body runs, so an inline seed would land too late. Mirrors the widget's
// export-globals.js / core-globals.js seed chain.

import * as FlamegraphApi from "../../../flamegraph_api.js";
import * as FlamegraphDiff from "../../../flamegraph_diff.js";
import { formatHumanDuration } from "../../../format.js";

const g = globalThis as Record<string, unknown>;
g["FlamegraphDiff"] ??= FlamegraphDiff;
g["FlamegraphApi"] ??= FlamegraphApi;
g["formatHumanDuration"] ??= formatHumanDuration;
