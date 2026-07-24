// Discovered service navigation. Browse rows remain unloaded until a service
// tab is activated, except for the sole-service auto-focus path.

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import type { ServiceMetadata } from "./state.js";

export function mountServiceTabs({ store, els, actions }: PageCtx): void {
  let lastServices: readonly string[] | null = null;
  let lastMetadata: readonly ServiceMetadata[] | null = null;

  store.subscribe(["browse", "ui"], (state) => {
    assertInScheduledRender("service-tabs render");
    const { services, serviceMetadata, activeService } = state.browse;
    const visible = state.ui.tab === "browse" && services.length > 0;
    els.serviceTabs.style.display = visible ? "" : "none";

    if (services !== lastServices || serviceMetadata !== lastMetadata) {
      lastServices = services;
      lastMetadata = serviceMetadata;
      els.serviceTabs.textContent = "";
      const hostCounts = new Map(
        serviceMetadata.map((metadata) => [metadata.service, metadata.host_count]),
      );
      for (const service of services) {
        const button = document.createElement("button");
        button.type = "button";
        button.role = "tab";
        button.dataset["service"] = service;
        button.append(document.createTextNode(service));
        const hostCount = hostCounts.get(service);
        if (hostCount != null) {
          const metadata = document.createElement("span");
          metadata.className = "service-tab-meta";
          metadata.textContent = `${hostCount} ${hostCount === 1 ? "host" : "hosts"}`;
          button.appendChild(metadata);
        }
        button.addEventListener("click", () => actions.selectService(service));
        els.serviceTabs.appendChild(button);
      }
    }

    for (const button of els.serviceTabs.querySelectorAll<HTMLButtonElement>("button")) {
      const active = button.dataset["service"] === activeService;
      button.classList.toggle("active", active);
      button.setAttribute("aria-selected", String(active));
    }
  });
}
