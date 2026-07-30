// Modal used by the Event inspector's numeric-field action. The metric-kind
// control is deliberately one cyclic button (Gauge -> Counter -> Up/down
// counter), matching the requested compact interaction.

import type { ViewerStore } from "../../store/store.js";
import type { FieldChartKind } from "../../types/state.js";
import { ESC_PRIORITY, type EscCascade } from "./esc-cascade.js";
import {
  addFieldChart,
  nextFieldChartKind,
} from "./field-chart-model.js";

export interface FieldChartDialog {
  open(eventName: string, fieldName: string): void;
  close(): void;
  isOpen(): boolean;
  dispose(): void;
}

const KIND_LABEL: Record<FieldChartKind, string> = {
  gauge: "Gauge",
  counter: "Counter",
  "updown-counter": "Up/down counter",
};

const KIND_HELP: Record<FieldChartKind, string> = {
  gauge: "A line joining observed values.",
  counter:
    "Deltas between successive observations, shown over each interval; decreases reset the baseline.",
  "updown-counter":
    "Signed deltas between successive observations, shown over each interval.",
};

interface PendingField {
  eventName: string;
  fieldName: string;
}

/** Mount one reusable, initially hidden dialog into `doc.body`. */
export function mountFieldChartDialog(
  doc: Document,
  store: ViewerStore,
  esc: EscCascade,
): FieldChartDialog {
  const backdrop = doc.createElement("div");
  backdrop.className = "d9-field-chart-backdrop";

  const form = doc.createElement("form");
  form.className = "d9-field-chart-dialog";
  form.setAttribute("role", "dialog");
  form.setAttribute("aria-modal", "true");
  form.setAttribute("aria-labelledby", "d9-field-chart-title");

  const title = doc.createElement("h2");
  title.id = "d9-field-chart-title";
  title.textContent = "Chart numeric field";

  const source = doc.createElement("div");
  source.className = "d9-field-chart-source";

  const selectorLabel = doc.createElement("span");
  selectorLabel.className = "d9-field-chart-kind-label";
  selectorLabel.textContent = "Metric kind";

  const selector = doc.createElement("button");
  selector.type = "button";
  selector.className = "d9-field-chart-kind";

  const help = doc.createElement("p");
  help.className = "d9-field-chart-help";
  help.id = "d9-field-chart-help";
  selector.setAttribute("aria-describedby", help.id);

  const actions = doc.createElement("div");
  actions.className = "d9-field-chart-actions";
  const cancel = doc.createElement("button");
  cancel.type = "button";
  cancel.textContent = "Cancel";
  const create = doc.createElement("button");
  create.type = "submit";
  create.className = "primary";
  create.textContent = "Create chart";
  actions.append(cancel, create);

  form.append(title, source, selectorLabel, selector, help, actions);
  backdrop.appendChild(form);
  doc.body.appendChild(backdrop);

  let pending: PendingField | null = null;
  let kind: FieldChartKind = "gauge";
  let restoreFocus: HTMLElement | null = null;

  function isOpen(): boolean {
    return backdrop.classList.contains("open");
  }

  function renderKind(): void {
    selector.textContent = `${KIND_LABEL[kind]} \u21bb`;
    selector.setAttribute(
      "aria-label",
      `Metric kind: ${KIND_LABEL[kind]}. Click to select the next kind.`,
    );
    help.textContent = KIND_HELP[kind];
  }

  function open(eventName: string, fieldName: string): void {
    pending = { eventName, fieldName };
    kind = "gauge";
    source.textContent = `${eventName} \u00b7 ${fieldName}`;
    renderKind();
    const active = doc.activeElement;
    restoreFocus = active instanceof HTMLElement ? active : null;
    backdrop.classList.add("open");
    selector.focus();
  }

  function close(): void {
    if (!isOpen()) return;
    backdrop.classList.remove("open");
    pending = null;
    restoreFocus?.focus();
    restoreFocus = null;
  }

  selector.addEventListener("click", () => {
    kind = nextFieldChartKind(kind);
    renderKind();
  });
  cancel.addEventListener("click", close);
  backdrop.addEventListener("mousedown", (event) => {
    if (event.target === backdrop) close();
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (pending === null) return;
    const chart = addFieldChart(
      store,
      pending.eventName,
      pending.fieldName,
      kind,
    );
    close();
    // The new track is appended after the built-ins. Reveal it after the
    // store-driven shell render creates its row.
    doc.defaultView?.requestAnimationFrame(() => {
      doc
        .querySelector(`[data-track-manage="${chart.id}"]`)
        ?.scrollIntoView({ block: "nearest" });
    });
  });
  form.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    event.stopPropagation();
    close();
  });

  const unregisterEsc = esc.register({
    name: "field-chart-dialog",
    priority: ESC_PRIORITY.popup,
    isOpen,
    close,
  });

  return {
    open,
    close,
    isOpen,
    dispose(): void {
      unregisterEsc();
      backdrop.remove();
    },
  };
}
