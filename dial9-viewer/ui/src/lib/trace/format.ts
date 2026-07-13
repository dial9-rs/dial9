// Typed re-export of the frozen format.js value-formatting helper.
// `formatFieldValue` renders a decoded span/event field value unit-aware
// (span metadata rows, span tooltip fields, the event detail sidebar). The
// import lives in lib/trace because the import-boundary check admits ui-root
// .js imports only here and in lib/canvas.

export { formatFieldValue } from "../../../format.js";
