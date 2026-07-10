// Minimal CLI argument parser shared by the parity entry points.
//
// Supports `--flag value` and `--flag` (boolean) forms plus positional
// arguments. No external dependency by design (constraint S1: playwright and
// axe-core are the only new devDependencies).

export function parseArgs(argv, spec = {}) {
  const opts = {};
  const positional = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const name = a.slice(2);
      const def = spec[name];
      if (def === undefined) {
        throw new Error(`unknown option --${name}`);
      }
      if (def.boolean) {
        opts[name] = true;
      } else {
        const v = argv[++i];
        if (v === undefined) throw new Error(`--${name} requires a value`);
        opts[name] = v;
      }
    } else {
      positional.push(a);
    }
  }
  for (const [name, def] of Object.entries(spec)) {
    if (opts[name] === undefined && def.default !== undefined) {
      opts[name] = def.default;
    }
    if (opts[name] === undefined && def.required) {
      throw new Error(`missing required option --${name}`);
    }
  }
  return { opts, positional };
}

export function usage(script, spec, positionalHelp = "") {
  const lines = [`usage: node ${script} ${positionalHelp}`.trimEnd()];
  for (const [name, def] of Object.entries(spec)) {
    const kind = def.boolean ? "" : " <value>";
    const dflt = def.default !== undefined ? ` (default: ${def.default})` : "";
    const req = def.required ? " (required)" : "";
    lines.push(`  --${name}${kind}  ${def.help ?? ""}${dflt}${req}`);
  }
  return lines.join("\n");
}
