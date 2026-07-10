// Readout capture: evaluate the readout schema (fixtures/readout-schema.mjs)
// against a live page. Used by the behavioral differ at every journey
// checkpoint.

export async function captureReadouts(page, fields) {
  const out = {};
  for (const f of fields) {
    out[f.id] = await captureField(page, f);
  }
  return out;
}

async function captureField(page, f) {
  switch (f.kind) {
    case "urlQuery": {
      const u = new URL(page.url());
      return u.search + u.hash;
    }
    case "count":
      return page.locator(f.selector).count();
    case "enabled": {
      const el = page.locator(f.selector).first();
      if ((await el.count()) === 0) return missing(f);
      return el.isEnabled();
    }
    case "value": {
      const el = page.locator(f.selector).first();
      if ((await el.count()) === 0) return missing(f);
      // Checkboxes read as checked-state, other inputs as value.
      return page.evaluate((sel) => {
        const e = document.querySelector(sel);
        return e.type === "checkbox" ? e.checked : e.value;
      }, f.selector);
    }
    case "text": {
      const el = page.locator(f.selector).first();
      if ((await el.count()) === 0) return missing(f);
      const text = ((await el.textContent()) ?? "").trim();
      if (f.regex) {
        const m = text.match(new RegExp(f.regex));
        return m ? m[1] : null;
      }
      return text;
    }
    default:
      throw new Error(`unknown readout kind "${f.kind}" (field ${f.id})`);
  }
}

function missing(f) {
  if (f.optional) return null;
  return "<MISSING ELEMENT>";
}

/**
 * Field-level exact diff of two readout captures (same schema). Volatile
 * fields are skipped. Returns [{ field, a, b }].
 */
export function diffReadouts(fields, a, b) {
  const diffs = [];
  for (const f of fields) {
    if (f.volatile) continue;
    const va = a[f.id];
    const vb = b[f.id];
    if (JSON.stringify(va) !== JSON.stringify(vb)) {
      diffs.push({ field: f.id, a: va, b: vb });
    }
  }
  return diffs;
}
