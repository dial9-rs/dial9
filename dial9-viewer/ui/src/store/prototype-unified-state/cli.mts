// PROTOTYPE ONLY. Thin terminal shell around model.ts.

import { createInterface } from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import {
  evolve,
  initialize,
  type Effect,
  type Event,
  type Model,
  type PageId,
  type Step,
} from "./model.mts";

const BOLD = "\x1b[1m";
const DIM = "\x1b[2m";
const RESET = "\x1b[0m";

const PAGE_KEYS: Readonly<Record<string, PageId>> = {
  "1": "browser",
  "2": "viewer",
  "3": "flamegraph",
  "4": "tokio-stats",
  "5": "span-explorer",
};

const SAMPLE_URLS: Record<PageId, string> = {
  browser:
    "https://prototype.invalid/browser?bucket=prod-traces&prefix=checkout&from=09%3A00&to=10%3A00&q=latency&tz=local&future=kept",
  viewer:
    "https://prototype.invalid/viewer?start=1000&end=9000&task=42&filter=poll&inspector=span&future=kept",
  flamegraph:
    "https://prototype.invalid/flamegraph?mode=api&service=checkout&host=a&host=b&max_files=96&worker-zoom=root%09poll&inspect=tokio%3A%3Aruntime&future=kept",
  "tokio-stats":
    "https://prototype.invalid/tokio-stats?start_ns=1000&end_ns=9000&tz=local&max_files=96&threshold_ms=25&tab=workers&future=kept",
  "span-explorer":
    "https://prototype.invalid/span-explorer?start_ns=1000&end_ns=9000&service=checkout&min_ms=5&max_ms=250&filter=status%3Derror&sort=duration&dir=desc&future=kept",
};

interface JournalEntry {
  readonly revision: number;
  readonly event: string;
  readonly outcome: Step["outcome"];
  readonly note: string;
}

interface Session {
  model: Model;
  effects: readonly Effect[];
  invalidated: readonly string[];
  journal: JournalEntry[];
}

function eventName(event: Event): string {
  if (event.type === "intent") return event.intent.type;
  return event.type;
}

function createSession(page: PageId, url?: string): Session {
  return {
    model: initialize(page, url),
    effects: [],
    invalidated: ["chrome", "content", "overlay"],
    journal: [
      {
        revision: 0,
        event: url === undefined ? "initialize/defaults" : "initialize/url",
        outcome: "committed",
        note: "defaults < URL, with no history echo",
      },
    ],
  };
}

function dispatch(session: Session, event: Event): Step {
  const step = evolve(session.model, event);
  session.model = step.model;
  session.effects = step.effects;
  session.invalidated = [...step.invalidated];
  session.journal.push({
    revision: step.model.revision,
    event: eventName(event),
    outcome: step.outcome,
    note: step.note,
  });
  if (session.journal.length > 8) session.journal.shift();
  return step;
}

function activeRequestId(model: Model): string | null {
  return model.operation.status === "running" ? model.operation.id : null;
}

function primaryIntent(model: Model): Event {
  switch (model.view.page) {
    case "browser":
      return {
        type: "intent",
        intent: {
          type: "browser/search-configured",
          bucket: "prod-traces",
          prefix: "payments",
          range: ["09:15", "10:15"],
          query: "spawn_blocking",
          timezone: "utc",
          tab: "raw",
          selectedRows: ["trace-17", "trace-21"],
        },
      };
    case "viewer":
      return {
        type: "intent",
        intent: {
          type: "viewer/analysis-changed",
          viewport: [2_000, 6_000],
          taskId: 73,
          filter: "spawn_blocking",
          inspector: "event",
          helpOpen: true,
        },
      };
    case "flamegraph":
      return {
        type: "intent",
        intent: {
          type: "flamegraph/analysis-configured",
          mode: "api",
          service: "payments",
          hosts: ["host-b", "host-a", "host-a"],
          maxFiles: 128,
          band: [3_000, 8_000],
          split: 0.65,
        },
      };
    case "tokio-stats":
      return {
        type: "intent",
        intent: {
          type: "tokio/query-configured",
          range: [2_000, 12_000],
          timezone: "utc",
          maxFiles: 128,
          thresholdMs: 50,
          tab: "resources",
        },
      };
    case "span-explorer":
      return {
        type: "intent",
        intent: {
          type: "span/query-configured",
          range: [2_000, 12_000],
          service: "payments",
          durationBandMs: [10, 500],
          filters: ["status=error", "name=database"],
          sort: ["duration", "desc"],
          selectedSpanId: "span-73",
          hiddenColumns: ["host", "parent"],
        },
      };
  }
}

function widgetProposal(model: Model, stale: boolean): Event {
  return {
    type: "flamegraph/widget-proposed",
    baseRevision: stale ? Math.max(0, model.revision - 1) : model.revision,
    workerZoom: ["root", "runtime", "poll"],
    offworkerZoom: ["root", "blocking"],
    inspectFocus: "tokio::runtime::task",
  };
}

function printableModel(model: Model): object {
  return {
    revision: model.revision,
    page: model.view.page,
    route: model.route,
    operation: model.operation,
    pointer: model.pointer,
    view: model.view.value,
  };
}

function render(session: Session, clear: boolean, title?: string): void {
  if (clear) console.clear();
  if (title !== undefined) {
    console.log(`${BOLD}${title}${RESET}`);
  }
  console.log(
    `${BOLD}PROTOTYPE: one authority for ${session.model.view.page}${RESET}`,
  );
  console.log(
    `${DIM}Question: can every meaningful page value move through one reducer without making resources/caches model state?${RESET}`,
  );
  console.log(JSON.stringify(printableModel(session.model), null, 2));
  console.log(`\n${BOLD}Effects emitted by last event${RESET}`);
  console.log(
    session.effects.length === 0
      ? `${DIM}(none)${RESET}`
      : JSON.stringify(session.effects, null, 2),
  );
  console.log(
    `${BOLD}Invalidated render lanes${RESET} ${session.invalidated.join(", ") || "(none)"}`,
  );
  console.log(`\n${BOLD}Recent transition journal${RESET}`);
  for (const entry of session.journal) {
    console.log(
      `${String(entry.revision).padStart(3)}  ${entry.outcome.padEnd(9)} ${entry.event.padEnd(34)} ${DIM}${entry.note}${RESET}`,
    );
  }
}

function renderKeys(): void {
  console.log(`\n${BOLD}Commands${RESET}`);
  console.log(
    `${BOLD}1-5${RESET} page  ${BOLD}h${RESET} hydrate URL  ${BOLD}e${RESET} page intent`,
  );
  console.log(
    `${BOLD}l${RESET} load  ${BOLD}p${RESET} progress  ${BOLD}c${RESET} complete  ${BOLD}s${RESET} stale completion`,
  );
  console.log(
    `${BOLD}m${RESET} pointer sample  ${BOLD}w${RESET} widget proposal  ${BOLD}W${RESET} stale widget proposal  ${BOLD}q${RESET} quit`,
  );
}

function renderInteractive(session: Session): void {
  render(session, true);
  renderKeys();
}

function runDemo(): void {
  const pages = Object.values(PAGE_KEYS);
  for (const page of pages) {
    const session = createSession(page, SAMPLE_URLS[page]);
    render(session, false, `\n=== ${page}: hydrated state ===`);

    const events: Event[] = [
      primaryIntent(session.model),
      { type: "load/requested" },
    ];
    for (const event of events) {
      dispatch(session, event);
      render(session, false, `--- after ${eventName(event)} ---`);
    }

    const requestId = activeRequestId(session.model);
    if (requestId === null) throw new Error("demo load did not start");
    const resultEvents: Event[] = [
      {
        type: "load/completed",
        requestId: "request-stale",
        summary: "must not commit",
      },
      {
        type: "load/progressed",
        requestId,
        sequence: 1,
        progress: "decoded 50%",
      },
      {
        type: "load/completed",
        requestId,
        summary: `${page} data ready`,
      },
      { type: "interaction/pointer-sampled", x: 640, y: 180 },
    ];
    for (const event of resultEvents) {
      dispatch(session, event);
      render(session, false, `--- after ${eventName(event)} ---`);
    }

    if (page === "flamegraph") {
      const proposals = [
        widgetProposal(session.model, false),
        widgetProposal(session.model, true),
      ];
      for (const event of proposals) {
        dispatch(session, event);
        render(session, false, `--- after ${eventName(event)} ---`);
      }
    }
  }
}

async function runInteractive(): Promise<void> {
  const reader = createInterface({ input, output });
  let session = createSession("viewer", SAMPLE_URLS.viewer);
  renderInteractive(session);

  for (;;) {
    const answer = (await reader.question("\n> ")).trim();
    if (answer === "q") break;

    const page = PAGE_KEYS[answer];
    if (page !== undefined) {
      session = createSession(page, SAMPLE_URLS[page]);
      renderInteractive(session);
      continue;
    }

    switch (answer) {
      case "h":
        dispatch(session, {
          type: "route/observed",
          url: SAMPLE_URLS[session.model.view.page],
        });
        break;
      case "e":
        dispatch(session, primaryIntent(session.model));
        break;
      case "l":
        dispatch(session, { type: "load/requested" });
        break;
      case "p": {
        const requestId = activeRequestId(session.model);
        if (requestId !== null) {
          dispatch(session, {
            type: "load/progressed",
            requestId,
            sequence: session.model.operation.status === "running"
              ? session.model.operation.sequence + 1
              : 1,
            progress: "next stream snapshot",
          });
        }
        break;
      }
      case "c": {
        const requestId = activeRequestId(session.model);
        if (requestId !== null) {
          dispatch(session, {
            type: "load/completed",
            requestId,
            summary: "current request completed",
          });
        }
        break;
      }
      case "s":
        dispatch(session, {
          type: "load/completed",
          requestId: "request-stale",
          summary: "must not commit",
        });
        break;
      case "m":
        dispatch(session, {
          type: "interaction/pointer-sampled",
          x: 100 + session.model.revision * 13,
          y: 80 + session.model.revision * 7,
        });
        break;
      case "w":
        dispatch(session, widgetProposal(session.model, false));
        break;
      case "W":
        dispatch(session, widgetProposal(session.model, true));
        break;
    }
    renderInteractive(session);
  }

  reader.close();
}

if (process.argv.includes("--demo")) {
  runDemo();
} else {
  await runInteractive();
}
