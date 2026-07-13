// Ambient declaration for the dual-UI switch global (ui-switch.js). New-UI
// entries load ui-switch.js via a plain <script src="/ui-switch.js"> tag (it
// is NOT bundled - the same copied file serves the legacy pages) and call
// window.D9UiSwitch.mount() from their entry module, so the global is the
// typed surface here rather than a module declaration.

interface D9UiSwitchApi {
  /**
   * Run the switch for this page. New-UI entries pass { side: "new" };
   * `page` overrides the reverse registry lookup when the entry path is
   * not the registered value verbatim.
   */
  mount(opts?: { side?: "new" | "legacy"; page?: string }): void;
}

interface Window {
  /** Present once /ui-switch.js has executed (loaded from <head>). */
  D9UiSwitch?: D9UiSwitchApi;
}
