// Shared mount context for the browser page's DOM components: each
// component owns its subtree, subscribes to the slices it renders, and
// dispatches actions on user input.

import type { BrowserActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import type { BrowserStore } from "./state.js";

export interface PageCtx {
  store: BrowserStore;
  els: BrowserEls;
  actions: BrowserActions;
}
