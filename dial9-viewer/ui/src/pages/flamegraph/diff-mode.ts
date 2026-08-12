// The two-sided differential flamegraph mode (`?diff=1&a=..&b=..`): side A
// (left) and side B (right) are independent aggregate scopes (each a
// base64url'd query). The frozen createDiffView self-mounts its DOM and owns
// the per-side SSE streams; this shell wires the credentials, the B-side
// cross-account re-prompt, and URL persistence, mirroring the legacy diff
// branch of flamegraph.html.

import { createDiffView, parseDiff } from "../../lib/canvas/index.js";
import type { DiffViewHandle, DiffViewState } from "../../lib/canvas/index.js";
import {
  Dial9Creds,
  Dial9Session,
  applyToCreds,
  isSourceShareable,
  readPlainSourceScope,
  sourceScopeFromStored,
} from "../../lib/trace/index.js";
import { mountCopyLink } from "../../lib/url/index.js";
import type { PageEls } from "./dom.js";
import { readDiffState, writeDiffState } from "./diff-state.js";

/** In-memory credentials for side B (never stored, never in the URL). */
interface SideBCreds {
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string;
  region?: string;
}

/** Build x-dial9-aws-* headers from an explicit credential set. */
function credsToHeaders(c: SideBCreds): Record<string, string> {
  const h: Record<string, string> = {
    "x-dial9-aws-access-key-id": c.accessKeyId,
    "x-dial9-aws-secret-access-key": c.secretAccessKey,
  };
  if (c.sessionToken) h["x-dial9-aws-session-token"] = c.sessionToken;
  if (c.region) h["x-dial9-aws-region"] = c.region;
  return h;
}

/**
 * Run the diff view for a `?diff=1` URL. Returns without rendering (falls back
 * to the caller) when the search is not a well-formed diff link.
 */
export function runDiffMode(params: URLSearchParams, els: PageEls): void {
  const diffParsed = parseDiff(params);
  if (diffParsed === null) {
    els.showError("Malformed diff link (missing or invalid a=/b= scopes).");
    return;
  }

  document.title = "Flamegraph diff";
  els.titleEl.textContent = "Flamegraph diff";
  els.loadingEl.classList.add("hidden");
  els.containerEl.style.display = "flex";

  const sourceParams = params.has("credential_mode") ? params : diffParsed.a;
  const source = readPlainSourceScope(
    sourceParams,
    sourceScopeFromStored("", Dial9Creds.get()),
  );
  applyToCreds(source, Dial9Creds);

  // The diff view persists onChange synchronously, so the URL is always current.
  const copyLink = mountCopyLink(els.headerEl);
  copyLink.el.style.display = isSourceShareable(source) ? "" : "none";

  // Per-side credentials: both default to this tab's session creds; `credsB`
  // overrides B once supplied (held in memory for this tab only).
  let credsB: SideBCreds | null = null;
  const headersFor = (side: "a" | "b"): Record<string, string> =>
    Dial9Session.headers(
      side === "b" && credsB !== null ? credsToHeaders(credsB) : Dial9Creds.headers(),
    );

  let view: DiffViewHandle | null = null;
  let bCredsPrompted = false;

  // Validate candidate B creds against B's bucket via /api/credentials/check
  // (both confirms access and resolves B's region for the S3 endpoint).
  const bBucket = diffParsed.b.get("bucket");
  async function checkBCreds(
    creds: SideBCreds,
  ): Promise<{ ok: boolean; region?: string | null; error?: string }> {
    const url =
      "/api/credentials/check" + (bBucket ? "?bucket=" + encodeURIComponent(bBucket) : "");
    const resp = await Dial9Session.fetch(url, {
      method: "POST",
      headers: credsToHeaders(creds),
    });
    if (!resp.ok) {
      return { ok: false, error: await resp.text().catch(() => "check failed") };
    }
    return (await resp.json()) as { ok: boolean; region?: string | null };
  }

  function promptForBCreds(reason: string): void {
    const back = document.createElement("div");
    back.className = "fgd-dialog-backdrop";
    back.innerHTML =
      '<div class="fgd-dialog">' +
      "<h3>Side B needs credentials</h3>" +
      "<p>The B capture is in a different account or bucket (<code>" +
      (bBucket ? bBucket.replace(/[<>&]/g, "") : "?") +
      "</code>). " +
      "Paste temporary AWS credentials for it (JSON). They are kept " +
      "in memory for this tab only — never stored, never put in the URL.</p>" +
      '<textarea rows="6" placeholder=\'{"accessKeyId":"...","secretAccessKey":"...","sessionToken":"..."}\'></textarea>' +
      '<div class="fgd-err"></div>' +
      '<div class="fgd-dialog-row"><button class="fgd-apply">Apply &amp; retry B</button>' +
      '<button class="ghost fgd-cancel">Cancel</button></div>' +
      "</div>";
    document.body.appendChild(back);
    const ta = back.querySelector("textarea") as HTMLTextAreaElement;
    const errEl = back.querySelector(".fgd-err") as HTMLElement;
    const applyBtn = back.querySelector(".fgd-apply") as HTMLButtonElement;
    if (reason) errEl.textContent = reason;
    ta.focus();
    (back.querySelector(".fgd-cancel") as HTMLElement).addEventListener("click", () =>
      back.remove(),
    );
    applyBtn.addEventListener("click", () => {
      void (async () => {
        let parsed: SideBCreds;
        try {
          parsed = Dial9Creds.parse(ta.value);
        } catch (e) {
          errEl.textContent = "Could not parse: " + (e instanceof Error ? e.message : String(e));
          return;
        }
        applyBtn.disabled = true;
        errEl.textContent = "Checking credentials…";
        const result = await checkBCreds(parsed);
        if (!result.ok) {
          applyBtn.disabled = false;
          errEl.textContent = "Rejected: " + (result.error || "could not access bucket");
          return;
        }
        // Carry the detected region so the listing hits the right endpoint.
        if (result.region && !parsed.region) parsed.region = result.region;
        credsB = parsed;
        back.remove();
        bCredsPrompted = false;
        view?.repollSide("b");
      })();
    });
  }

  function onSideError(side: "a" | "b", err: Error & { status?: number }): void {
    const auth = err.status === 401 || err.status === 403;
    if (side === "b" && auth && !bCredsPrompted) {
      bCredsPrompted = true;
      promptForBCreds(err.message);
      return;
    }
    els.statsEl.textContent = "Side " + side.toUpperCase() + " failed: " + (err.message || String(err));
  }

  // Persist the diff view's zoom + highlight to the URL. The scope keys
  // (diff/a/b) are already in the address bar; writeDiffState only touches its
  // own diff_zoom/diff_search keys, so it composes with them.
  function onDiffViewChange(st: DiffViewState): void {
    const p = new URLSearchParams(window.location.search);
    writeDiffState(p, st);
    history.replaceState(null, "", window.location.pathname + "?" + p.toString());
  }

  view = createDiffView(els.containerEl, {
    scopeA: diffParsed.a,
    scopeB: diffParsed.b,
    headersFor,
    onSideError,
    onChange: onDiffViewChange,
    initialState: readDiffState(params),
  });
  els.statsEl.textContent =
    "A (left) vs B (right) · blue = heavier in A, red = heavier in B";
}
