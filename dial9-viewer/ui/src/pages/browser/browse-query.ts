export interface BrowseQuery {
  bucket: string;
  from: number;
  to: number;
  prefix?: string | undefined;
  service?: string | undefined;
  layoutHint?: string | undefined;
}

export interface ServicesQuery {
  bucket: string;
  from: number;
  to: number;
  prefix?: string | undefined;
}

/** Build the Browse-tab request. Empty optional filters are omitted. */
export function buildBrowseUrl(query: BrowseQuery): string {
  let url =
    `/api/browse?bucket=${encodeURIComponent(query.bucket)}` +
    `&from=${query.from}&to=${query.to}`;
  if (query.prefix) url += `&prefix=${encodeURIComponent(query.prefix)}`;
  if (query.service) url += `&service=${encodeURIComponent(query.service)}`;
  if (query.layoutHint) url += `&layout_hint=${encodeURIComponent(query.layoutHint)}`;
  return url;
}

/** Build the lightweight service-discovery request. */
export function buildServicesUrl(query: ServicesQuery): string {
  let url =
    `/api/services?bucket=${encodeURIComponent(query.bucket)}` +
    `&from=${query.from}&to=${query.to}`;
  if (query.prefix) url += `&prefix=${encodeURIComponent(query.prefix)}`;
  return url;
}

export interface ServiceSelection {
  active: string | null;
  shouldLoad: boolean;
}

/** Tab clicks stay constrained to discovered services; URL history entries may
 * directly request a service that is absent from the current tab list. */
export function canSelectService(
  services: readonly string[],
  requested: string,
  fromHistory: boolean,
): boolean {
  return requested.trim() !== "" && (fromHistory || services.includes(requested));
}

/**
 * Resolve a service already selected by the URL before discovery. Returning a
 * selection tells the caller to skip `/api/services` and browse it directly.
 */
export function resolveRequestedService(
  requested: string,
): ServiceSelection | null {
  const active = requested.trim();
  return active ? { active, shouldLoad: true } : null;
}

/**
 * Resolve discovery into a focused service. A sole result opens
 * automatically; multiple results stay lazy unless the URL requested one.
 */
export function resolveServiceSelection(
  services: readonly string[],
  requested: string,
): ServiceSelection {
  if (services.length === 1) {
    return { active: services[0]!, shouldLoad: true };
  }
  if (requested && services.includes(requested)) {
    return { active: requested, shouldLoad: true };
  }
  return { active: null, shouldLoad: false };
}
