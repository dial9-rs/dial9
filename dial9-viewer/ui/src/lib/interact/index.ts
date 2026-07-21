// lib/interact barrel: the unified keyboard model's mechanism. Pages
// import from here; interaction modules translate raw events into store
// actions / component callbacks only - they never render page content (the
// palette/help/toast surfaces they own are chrome, not trace rendering).

export {
  createKeyRouter,
  isTextEntryTarget,
  mountKeyRouter,
} from "./keyboard.js";
export type {
  KeyBinding,
  KeyEventLike,
  KeyEventSource,
  KeyRouter,
} from "./keyboard.js";

export { createZoomHistory } from "./zoom-history.js";
export type { ZoomHistory, ZoomHistoryOptions } from "./zoom-history.js";

export { createPointerMachine, panWindowByPixels, DRAG_INTENT_PX } from "./pointer.js";
export type {
  PointerMachine,
  PointerCommand,
  PointerDownInput,
  PointerMoveInput,
  DragSnapshot,
  GestureKind,
} from "./pointer.js";

export { wheelZoomIntent, WHEEL_ZOOM_FACTOR } from "./wheel.js";
export type { WheelInput, WheelZoomIntent } from "./wheel.js";

export { createKbSelectionMachine, MIN_SELECTION_NS } from "./kb-selection.js";
export type {
  KbSelectionMachine,
  KbSelectionCommand,
  SelectionMode,
  ExtendDirection,
} from "./kb-selection.js";

export {
  createSearchPalette,
  filterPaletteItems,
  stepClamped,
  stepWrapped,
} from "./palette.js";
export type { PaletteItem, PaletteOptions, SearchPalette } from "./palette.js";

export { createHelpOverlay, helpKeyBindings } from "./help-overlay.js";
export type {
  HelpOverlay,
  HelpOverlayOptions,
  HelpRow,
  HelpSection,
} from "./help-overlay.js";

export { parseGotoTime } from "./goto-time.js";
export type { GotoTime } from "./goto-time.js";

export { createAnnouncer, getAnnouncer } from "./announce.js";
export type { Announcer } from "./announce.js";
