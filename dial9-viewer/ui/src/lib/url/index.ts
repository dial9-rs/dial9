// Barrel for the URL view-state layer: the versioned view-state codec, the
// store->URL sync binding, and the copy-link action.

export {
  VIEW_STATE_VERSION,
  LEGACY_WORKER_ZOOM_PARAM,
  LEGACY_OFFWORKER_ZOOM_PARAM,
  encodeViewState,
  decodeViewState,
  legacyZoomFromQuery,
  applyLegacyZoomToQuery,
  resolveViewState,
} from "./view-state.js";
export type {
  ViewState,
  DecodedViewState,
  UnknownEntry,
  TimeMode,
  TimeZoneMode,
} from "./view-state.js";

export { bindViewStateToUrl, windowUrlHost, DEFAULT_DEBOUNCE_MS } from "./sync.js";
export type {
  BindViewStateOptions,
  ViewStateBinding,
  UrlHost,
  UrlParts,
  DebounceTimer,
} from "./sync.js";

export { mountCopyLink } from "./copy-link.js";
export type { CopyLinkOptions, CopyLinkHandle } from "./copy-link.js";
