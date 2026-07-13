// Shared renderer for the two status areas (#browse-status / #raw-status):
// plain text for simple messages, or a lead line + <br> + one styled
// <code> per sample key (the empty-result hint). Built with createElement /
// textContent - no HTML interpolation.

import type { StatusState } from "./state.js";

export function renderStatus(el: HTMLElement, status: StatusState): void {
  el.className = "status" + (status.kind === "error" ? " error" : "");
  el.style.display = status.visible ? "" : "none";
  if (status.sampleKeys) {
    el.textContent = "";
    el.append(document.createTextNode(status.text), document.createElement("br"));
    status.sampleKeys.forEach((key, i) => {
      if (i > 0) el.append(document.createElement("br"));
      const code = document.createElement("code");
      code.style.cssText = "color:#7ec8e3;font-size:0.85em";
      code.textContent = key;
      el.append(code);
    });
  } else {
    el.textContent = status.text;
  }
}
