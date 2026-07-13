// lib/trace/format.ts - typed re-export of the frozen format.js
// value-formatting helpers used by the trace-domain UI (T26).
//
// `formatFieldValue` renders a decoded span/event field value unit-aware
// (viewer.html span metadata rows J3, span tooltip fields J6, and the event
// detail sidebar). It lives in the frozen core (format.js) alongside
// formatHumanDuration/formatHumanBytes; those two already reach src/ through
// ./api_format.ts (formatHumanDuration) and lib/canvas, so this seam adds
// only the field formatter. The import lives in lib/trace because the
// import-boundary check (scripts/check-core-imports.mjs) admits ui-root .js
// imports only here and in lib/canvas.

export { formatFieldValue } from "../../../format.js";
