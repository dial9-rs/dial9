// Global custom-event field catalogue plus the metric-kind picker shared by
// catalogue rows and the Event inspector's contextual chart shortcut.

import type { ViewerStore } from "../../store/store.js";
import type { FieldChartKind } from "../../types/state.js";
import type { ParsedTrace } from "../../lib/trace/index.js";
import { ESC_PRIORITY, type EscCascade } from "./esc-cascade.js";
import {
  buildFieldChartCatalog,
  fieldChartCatalogSize,
  filterFieldChartCatalog,
  type FieldChartCatalog,
  type FieldChartSource,
  type FieldChartSourceGroup,
} from "./field-chart-catalog.js";
import { addFieldChart, FIELD_CHART_KINDS } from "./field-chart-model.js";

export interface FieldChartDialog {
  /** Open the kind picker for an unannotated contextual field. */
  open(eventName: string, fieldName: string): void;
  /** Browse every graphable field in the loaded trace. */
  openCatalog(): void;
  /** Create without opening the dialog when schema metadata supplies the kind. */
  create(eventName: string, fieldName: string, kind: FieldChartKind): void;
  close(): void;
  isOpen(): boolean;
  dispose(): void;
}

interface KindOptionContent {
  label: string;
  description: string;
  paths: readonly string[];
}

type KindOption = KindOptionContent & { kind: FieldChartKind };

const KIND_OPTION_CONTENT: Record<FieldChartKind, KindOptionContent> = {
  gauge: {
    label: "Gauge",
    description: "Observed values",
    paths: ["M2 2v12h12", "M3.5 10.5 6.5 7 9 8.5 13 4"],
  },
  counter: {
    label: "Counter",
    description: "Positive deltas",
    paths: ["M2 13.5h12", "M4 12V8M8 12V4M12 12V6"],
  },
  "updown-counter": {
    label: "Up/down counter",
    description: "Signed deltas",
    paths: ["M2 8h12", "M4 8V3M8 8v5M12 8V5"],
  },
};

// Derived from the same exhaustive kind list the URL decoder accepts, so a
// newly supported kind cannot silently disappear from this visible picker.
const KIND_OPTIONS: readonly KindOption[] = FIELD_CHART_KINDS.map((kind) => ({
  kind,
  ...KIND_OPTION_CONTENT[kind],
}));

type DialogMode =
  | { type: "catalog" }
  | {
      type: "kind";
      eventName: string;
      fieldName: string;
      returnToCatalog: boolean;
    };

/** Mount one reusable, initially hidden dialog into `doc.body`. */
export function mountFieldChartDialog(
  doc: Document,
  store: ViewerStore,
  esc: EscCascade,
): FieldChartDialog {
  const catalogs = new WeakMap<ParsedTrace, FieldChartCatalog>();
  const getCatalog = (): FieldChartCatalog => {
    const trace = store.getState().trace.trace;
    if (trace === null) return { annotated: [], other: [] };
    let catalog = catalogs.get(trace);
    if (catalog === undefined) {
      catalog = buildFieldChartCatalog(trace.customEvents ?? []);
      catalogs.set(trace, catalog);
    }
    return catalog;
  };

  const backdrop = doc.createElement("div");
  backdrop.className = "d9-field-chart-backdrop";

  const dialog = doc.createElement("div");
  dialog.className = "d9-field-chart-dialog";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-labelledby", "d9-field-chart-title");

  const form = doc.createElement("form");
  form.className = "d9-field-chart-form";

  const title = doc.createElement("h2");
  title.id = "d9-field-chart-title";

  const body = doc.createElement("div");
  body.className = "d9-field-chart-body";

  const actions = doc.createElement("div");
  actions.className = "d9-field-chart-actions";
  const back = doc.createElement("button");
  back.type = "button";
  back.textContent = "Back";
  const cancel = doc.createElement("button");
  cancel.type = "button";
  cancel.className = "d9-field-chart-cancel";
  const create = doc.createElement("button");
  create.type = "submit";
  create.className = "primary";
  create.textContent = "Create";
  actions.append(back, cancel, create);

  form.append(title, body, actions);
  dialog.appendChild(form);
  backdrop.appendChild(dialog);
  doc.body.appendChild(backdrop);

  let mode: DialogMode | null = null;
  let kind: FieldChartKind = "gauge";
  let query = "";
  let otherExpanded = false;
  let restoreFocus: HTMLElement | null = null;

  function isOpen(): boolean {
    return backdrop.classList.contains("open");
  }

  function beginOpen(): void {
    if (isOpen()) return;
    const active = doc.activeElement;
    restoreFocus = active instanceof HTMLElement ? active : null;
    backdrop.classList.add("open");
  }

  function close(): void {
    if (!isOpen()) return;
    backdrop.classList.remove("open");
    mode = null;
    body.replaceChildren();
    restoreFocus?.focus();
    restoreFocus = null;
  }

  function createChart(
    eventName: string,
    fieldName: string,
    chartKind: FieldChartKind,
  ): void {
    const chart = addFieldChart(store, eventName, fieldName, chartKind);
    // The new track is appended after the built-ins. Reveal it after the
    // store-driven shell render creates its row.
    doc.defaultView?.requestAnimationFrame(() => {
      doc
        .querySelector(`[data-track-manage="${chart.id}"]`)
        ?.scrollIntoView({ block: "nearest" });
    });
  }

  function openCatalog(): void {
    mode = { type: "catalog" };
    query = "";
    otherExpanded = false;
    beginOpen();
    renderCatalog();
  }

  function renderCatalog(): void {
    dialog.classList.add("is-catalog");
    title.textContent = "Add field chart";
    back.hidden = true;
    create.hidden = true;
    cancel.textContent = "Close";
    body.replaceChildren();

    const intro = doc.createElement("p");
    intro.className = "d9-field-chart-intro";
    intro.textContent =
      "Browse numeric custom-event fields. Annotated fields create a chart in one click.";

    const search = doc.createElement("input");
    search.className = "d9-field-chart-search";
    search.type = "search";
    search.placeholder = "Filter by event or field...";
    search.setAttribute("aria-label", "Filter graphable fields");
    search.autocomplete = "off";
    search.spellcheck = false;
    search.value = query;

    const status = doc.createElement("div");
    status.className = "d9-sr-only";
    status.setAttribute("role", "status");
    status.setAttribute("aria-live", "polite");

    const results = doc.createElement("div");
    results.className = "d9-field-chart-catalog";
    results.setAttribute("aria-label", "Graphable fields");

    const catalog = getCatalog();
    const update = (): void => {
      query = search.value;
      renderCatalogResults(results, status, catalog);
    };
    search.addEventListener("input", update);
    body.append(intro, search, status, results);
    update();
    search.focus();
  }

  function renderCatalogResults(
    results: HTMLElement,
    status: HTMLElement,
    catalog: FieldChartCatalog,
  ): void {
    const filtered = filterFieldChartCatalog(catalog, query);
    const total = fieldChartCatalogSize(filtered);
    status.textContent = `${total} graphable field${total === 1 ? "" : "s"}`;
    results.replaceChildren();

    if (fieldChartCatalogSize(catalog) === 0) {
      results.appendChild(emptyMessage("No graphable custom-event fields found."));
      return;
    }
    if (total === 0) {
      results.appendChild(emptyMessage(`No fields match “${query.trim()}”.`));
      return;
    }

    if (filtered.annotated.length > 0 || query.trim() === "") {
      const section = doc.createElement("section");
      section.className = "d9-field-chart-section";
      section.setAttribute("aria-label", "Annotated fields");
      const heading = doc.createElement("h3");
      heading.textContent = "Annotated fields";
      section.appendChild(heading);
      if (filtered.annotated.length > 0) {
        appendSourceGroups(section, filtered.annotated);
      } else {
        section.appendChild(emptyMessage("No annotated fields in this trace."));
      }
      results.appendChild(section);
    }

    if (filtered.other.length === 0) return;
    const details = doc.createElement("details");
    details.className = "d9-field-chart-other";
    details.open = query.trim() !== "" || otherExpanded;
    const summary = doc.createElement("summary");
    const otherCount = filtered.other.reduce(
      (sum, group) => sum + group.fields.length,
      0,
    );
    summary.textContent = `All other fields (${otherCount})`;
    details.appendChild(summary);
    appendSourceGroups(details, filtered.other);
    details.addEventListener("toggle", () => {
      if (query.trim() === "") otherExpanded = details.open;
    });
    results.appendChild(details);
  }

  function emptyMessage(text: string): HTMLElement {
    const message = doc.createElement("p");
    message.className = "d9-field-chart-empty";
    message.textContent = text;
    return message;
  }

  function appendSourceGroups(
    parent: HTMLElement,
    groups: readonly FieldChartSourceGroup[],
  ): void {
    for (const group of groups) {
      const groupEl = doc.createElement("div");
      groupEl.className = "d9-field-chart-source-group";
      groupEl.setAttribute("role", "group");
      groupEl.setAttribute("aria-label", group.eventName);
      const eventName = doc.createElement("div");
      eventName.className = "d9-field-chart-event";
      eventName.textContent = group.eventName;
      groupEl.appendChild(eventName);
      for (const source of group.fields) {
        groupEl.appendChild(sourceButton(source));
      }
      parent.appendChild(groupEl);
    }
  }

  function sourceButton(source: FieldChartSource): HTMLButtonElement {
    const button = doc.createElement("button");
    button.type = "button";
    button.className = "d9-field-chart-source-button";
    button.dataset.fieldChartEvent = source.eventName;
    button.dataset.fieldChartField = source.fieldName;
    const field = doc.createElement("span");
    field.className = "d9-field-chart-field";
    field.textContent = source.fieldName;
    const meta = doc.createElement("span");
    meta.className = source.kind !== null
      ? "d9-field-chart-kind-badge"
      : "d9-field-chart-choose";
    const kindLabel =
      source.kind === null ? null : KIND_OPTION_CONTENT[source.kind].label;
    meta.textContent = kindLabel ?? "Choose type";
    button.append(field, meta);
    button.setAttribute(
      "aria-label",
      source.kind === null
        ? `Choose chart type for ${source.eventName}.${source.fieldName}`
        : `Create ${source.eventName}.${source.fieldName} as ${kindLabel}`,
    );
    button.addEventListener("click", () => {
      if (source.kind !== null) {
        createChart(source.eventName, source.fieldName, source.kind);
        close();
      } else {
        showKindPicker(source.eventName, source.fieldName, true);
      }
    });
    return button;
  }

  function open(eventName: string, fieldName: string): void {
    beginOpen();
    showKindPicker(eventName, fieldName, false);
  }

  function showKindPicker(
    eventName: string,
    fieldName: string,
    returnToCatalog: boolean,
  ): void {
    mode = { type: "kind", eventName, fieldName, returnToCatalog };
    kind = "gauge";
    dialog.classList.remove("is-catalog");
    title.textContent = "Choose chart type";
    back.hidden = !returnToCatalog;
    create.hidden = false;
    cancel.textContent = "Cancel";
    body.replaceChildren();

    const source = doc.createElement("div");
    source.className = "d9-field-chart-source";
    source.textContent = `${eventName} \u00b7 ${fieldName}`;
    const prompt = doc.createElement("p");
    prompt.className = "d9-field-chart-kind-label";
    prompt.textContent = "Interpret this field as";
    const options = doc.createElement("div");
    options.className = "d9-field-chart-kind-options";
    options.setAttribute("role", "group");
    options.setAttribute("aria-label", "Chart type");

    const buttons = KIND_OPTIONS.map((option) => {
      const button = doc.createElement("button");
      button.type = "button";
      button.className = "d9-field-chart-kind";
      button.dataset.kind = option.kind;
      button.append(kindIcon(option), optionText(option));
      button.addEventListener("click", () => {
        kind = option.kind;
        updateKindSelection(buttons);
      });
      options.appendChild(button);
      return button;
    });
    body.append(source, prompt, options);
    updateKindSelection(buttons);
    buttons[0]?.focus();
  }

  function kindIcon(option: KindOption): SVGSVGElement {
    const svg = doc.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 16 16");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "1.5");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    for (const d of option.paths) {
      const path = doc.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", d);
      svg.appendChild(path);
    }
    return svg;
  }

  function optionText(option: KindOption): HTMLElement {
    const text = doc.createElement("span");
    text.className = "d9-field-chart-kind-text";
    const label = doc.createElement("span");
    label.className = "d9-field-chart-kind-name";
    label.textContent = option.label;
    const description = doc.createElement("span");
    description.className = "d9-field-chart-kind-description";
    description.textContent = option.description;
    text.append(label, description);
    return text;
  }

  function updateKindSelection(buttons: readonly HTMLButtonElement[]): void {
    for (const button of buttons) {
      const selected = button.dataset.kind === kind;
      button.classList.toggle("selected", selected);
      button.setAttribute("aria-pressed", String(selected));
    }
  }

  back.addEventListener("click", () => {
    if (mode?.type !== "kind" || !mode.returnToCatalog) return;
    mode = { type: "catalog" };
    renderCatalog();
  });
  cancel.addEventListener("click", close);
  backdrop.addEventListener("mousedown", (event) => {
    if (event.target === backdrop) close();
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (mode?.type !== "kind") return;
    createChart(mode.eventName, mode.fieldName, kind);
    close();
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
    openCatalog,
    create: createChart,
    close,
    isOpen,
    dispose(): void {
      unregisterEsc();
      backdrop.remove();
    },
  };
}
