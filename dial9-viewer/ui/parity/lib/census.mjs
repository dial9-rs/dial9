// Affordance census: capture + diff (T12 layer b).
//
// The census enumerates every DOM affordance on a page — the interactive
// controls a user can operate — in document order, with the attributes that
// define the affordance from the outside: what it is (tag/type/role), what it
// says (label/title/placeholder), where it goes (href), and whether it is
// currently operable (visible/disabled). This is the "33 live DOM
// affordances" idea from features/01's Live-validation Outcome, productized.
//
// What it deliberately does NOT capture: styling (classes change across the
// migration), values (behavioral state — layer (c)'s readouts own those), and
// canvas-internal hit targets (not DOM; covered by row walkers + journeys).
//
// Identity for diffing: `#id` when the element has one, else
// `tag[role]:label`; duplicate identities get a `~n` occurrence suffix (the
// legacy pages DO contain duplicate ids — an axe finding — so even id keys
// are disambiguated).

// Roles that make a non-native element an affordance.
const INTERACTIVE_ROLES = [
  "button",
  "link",
  "tab",
  "checkbox",
  "radio",
  "menuitem",
  "menuitemcheckbox",
  "menuitemradio",
  "switch",
  "slider",
  "combobox",
  "listbox",
  "option",
  "searchbox",
  "spinbutton",
  "textbox",
];

/**
 * Capture the census of the current page. Returns [{ key, tag, id, type,
 * role, label, title, placeholder, href, disabled, visible }] in document
 * order. Run AFTER the page's loaded-wait so load-time affordances
 * (config-gated buttons, discovered prefix chips) are present.
 */
export async function captureCensus(page) {
  const raw = await page.evaluate((interactiveRoles) => {
    const selector = [
      "a[href]",
      "button",
      "input",
      "select",
      "textarea",
      "summary",
      "[onclick]",
      ...interactiveRoles.map((r) => `[role="${r}"]`),
    ].join(", ");

    const collapse = (s) => (s ?? "").replace(/\s+/g, " ").trim();

    return [...document.querySelectorAll(selector)].map((el) => {
      const tag = el.tagName.toLowerCase();
      const isButtonLike = tag === "button" || tag === "a" || tag === "summary";
      const inputButton = tag === "input" && /^(button|submit|reset)$/.test(el.type ?? "");
      const ownText = isButtonLike || el.getAttribute("role") ? collapse(el.textContent) : "";
      const label =
        collapse(el.getAttribute("aria-label")) ||
        ownText ||
        (inputButton ? collapse(el.value) : "") ||
        null;
      const rects = el.getClientRects().length > 0;
      const visible = rects && getComputedStyle(el).visibility !== "hidden";
      return {
        tag,
        id: el.id || null,
        type: tag === "input" ? el.type || null : null,
        role: el.getAttribute("role") || null,
        label,
        title: collapse(el.getAttribute("title")) || null,
        placeholder: collapse(el.getAttribute("placeholder")) || null,
        // Raw attribute (not resolved), so two hosts serving the same page
        // compare equal.
        href: tag === "a" ? el.getAttribute("href") : null,
        disabled: "disabled" in el ? !!el.disabled : false,
        visible,
      };
    });
  }, INTERACTIVE_ROLES);

  // Assign diff identities, disambiguating duplicates by occurrence.
  const counts = new Map();
  for (const e of raw) {
    const base = e.id
      ? `#${e.id}`
      : `${e.tag}${e.role ? `[${e.role}]` : ""}:${e.label ?? e.title ?? e.placeholder ?? e.href ?? ""}`;
    const n = counts.get(base) ?? 0;
    counts.set(base, n + 1);
    e.key = n === 0 ? base : `${base}~${n}`;
  }
  return raw;
}

// The fields an affordance is compared on (everything captured except `key`).
const DIFF_FIELDS = [
  "tag",
  "id",
  "type",
  "role",
  "label",
  "title",
  "placeholder",
  "href",
  "disabled",
  "visible",
];

/**
 * Diff two censuses (A = reference, B = candidate). Returns
 *   { added: [entryB...], removed: [entryA...],
 *     changed: [{ key, fields: { field: { a, b } } }...] }
 * Empty arrays everywhere = zero diff.
 */
export function diffCensus(a, b) {
  const byKey = (list) => new Map(list.map((e) => [e.key, e]));
  const am = byKey(a);
  const bm = byKey(b);

  const removed = a.filter((e) => !bm.has(e.key));
  const added = b.filter((e) => !am.has(e.key));
  const changed = [];
  for (const [key, ea] of am) {
    const eb = bm.get(key);
    if (!eb) continue;
    const fields = {};
    for (const f of DIFF_FIELDS) {
      if (ea[f] !== eb[f]) fields[f] = { a: ea[f], b: eb[f] };
    }
    if (Object.keys(fields).length) changed.push({ key, fields });
  }
  return { added, removed, changed };
}

export function censusDiffCount(diff) {
  return diff.added.length + diff.removed.length + diff.changed.length;
}

/** One-line-per-entry markdown table of a census dump. */
export function censusTable(census) {
  const lines = [
    "| # | Key | Tag | Type/Role | Label | Visible | Disabled |",
    "|---|---|---|---|---|---|---|",
  ];
  const esc = (v) => (v == null ? "" : String(v).replace(/\|/g, "\\|"));
  census.forEach((e, i) => {
    lines.push(
      `| ${i + 1} | ${esc(e.key)} | ${e.tag} | ${esc(e.type ?? e.role)} | ${esc(e.label)} | ${e.visible ? "yes" : "no"} | ${e.disabled ? "yes" : "no"} |`,
    );
  });
  return lines.join("\n");
}

/** Human-readable diff rendering (markdown). */
export function censusDiffReport(diff) {
  const lines = [];
  for (const e of diff.removed) lines.push(`- REMOVED ${e.key} (${e.tag}, label: ${JSON.stringify(e.label)})`);
  for (const e of diff.added) lines.push(`- ADDED   ${e.key} (${e.tag}, label: ${JSON.stringify(e.label)})`);
  for (const c of diff.changed) {
    const fields = Object.entries(c.fields)
      .map(([f, { a, b }]) => `${f}: ${JSON.stringify(a)} -> ${JSON.stringify(b)}`)
      .join(", ");
    lines.push(`- CHANGED ${c.key}: ${fields}`);
  }
  return lines.length ? lines.join("\n") : "(zero diff)";
}
