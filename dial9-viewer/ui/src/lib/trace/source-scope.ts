// The source + credential identity for every viewer surface: which bucket to
// read, in which region, and which ONE credential transport to use. Keeping
// these together makes the invalid "literal keys plus a role ARN" state
// unrepresentable and stops each page from independently reconstructing source
// identity.
//
// The transport rule comes from the server credential parser:
//
//   • Literal credentials and role credentials travel as x-dial9-aws-* headers.
//   • A role ARN may arrive in a header OR the aws_role_arn query parameter, but
//     never both. Runtime API URLs therefore omit the role; browser/share URLs
//     carry it so a fresh tab can restore the identity before making requests.
//   • Region remains on API URLs because ambient cross-region reads have no
//     credential object to carry an x-dial9-aws-region header.
//
// Browser URLs always carry an explicit credential mode. Literal values never
// do: a literal URL marker means "use this tab's stored literal credentials" or
// "show the literal credential form" when no usable keys are stored.

export interface AmbientCredentials {
  kind: "ambient";
}

export interface LiteralCredentials {
  kind: "literal";
  accessKeyId: string;
  secretAccessKey: string;
  sessionToken?: string | undefined;
}

export interface RoleCredentials {
  kind: "role";
  roleArn: string;
}

export type SourceCredentials =
  | AmbientCredentials
  | LiteralCredentials
  | RoleCredentials;

export interface SourceScope {
  /** S3 bucket; "" when absent (for example local/demo sources). */
  bucket: string;
  /** AWS region the bucket lives in; "" when unknown/server-default. */
  region: string;
  /** Exactly one credential mode. */
  credentials: SourceCredentials;
}

export type UrlCredentials =
  | AmbientCredentials
  | { kind: "literal" }
  | RoleCredentials;

export interface UrlSourceScope {
  bucket: string;
  region: string;
  credentials: UrlCredentials;
}

export type ShareableCredentials = AmbientCredentials | RoleCredentials;

export interface ShareableSourceScope {
  bucket: string;
  region: string;
  credentials: ShareableCredentials;
}

export type Shareability =
  | { kind: "shareable"; scope: ShareableSourceScope }
  | { kind: "literal-credentials" };

export const AMBIENT_CREDENTIALS: AmbientCredentials = { kind: "ambient" };
export const EMPTY_LITERAL_CREDENTIALS: LiteralCredentials = {
  kind: "literal",
  accessKeyId: "",
  secretAccessKey: "",
};
export const EMPTY_SOURCE_SCOPE: SourceScope = {
  bucket: "",
  region: "",
  credentials: AMBIENT_CREDENTIALS,
};

export type StoredSourceCredentials = SourceCredentials & {
  region?: string | undefined;
};

/** Build a normalized source scope from explicit credential state. */
export function makeSourceScope(
  bucket: string | null | undefined,
  region: string | null | undefined,
  credentials: SourceCredentials = AMBIENT_CREDENTIALS,
): SourceScope {
  return {
    bucket: bucket || "",
    region: region || "",
    credentials,
  };
}

/** Build the source fallback represented by the tab-scoped credential store. */
export function sourceScopeFromStored(
  bucket: string | null | undefined,
  stored: StoredSourceCredentials,
): SourceScope {
  const { region = "", ...credentials } = stored;
  return makeSourceScope(bucket, region, credentials as SourceCredentials);
}

export function credentialMode(scope: SourceScope): SourceCredentials["kind"] {
  return scope.credentials.kind;
}

export function isLiteralConfigured(credentials: SourceCredentials): boolean {
  return (
    credentials.kind === "literal" &&
    credentials.accessKeyId.trim() !== "" &&
    credentials.secretAccessKey.trim() !== ""
  );
}

/** URL-safe projection. Literal mode deliberately drops every secret value. */
export function toUrlSourceScope(scope: SourceScope): UrlSourceScope {
  switch (scope.credentials.kind) {
    case "ambient":
      return { bucket: scope.bucket, region: scope.region, credentials: AMBIENT_CREDENTIALS };
    case "literal":
      return { bucket: scope.bucket, region: scope.region, credentials: { kind: "literal" } };
    case "role":
      return {
        bucket: scope.bucket,
        region: scope.region,
        credentials: { kind: "role", roleArn: scope.credentials.roleArn },
      };
  }
}

/** Built-in sharing is intentionally unavailable for literal credentials. */
export function toShareableSourceScope(scope: SourceScope): Shareability {
  if (scope.credentials.kind === "literal") return { kind: "literal-credentials" };
  return {
    kind: "shareable",
    scope: {
      bucket: scope.bucket,
      region: scope.region,
      credentials: scope.credentials,
    },
  };
}

export function isSourceShareable(scope: SourceScope): boolean {
  return scope.credentials.kind !== "literal";
}

// `s_*` is the trace viewer's compact-scope vocabulary. Plain names are used
// by the browser and aggregate pages. `credential_mode` is frontend-only; API
// request writers intentionally never emit it.
const NAMESPACED = {
  bucket: "s_bucket",
  region: "s_region",
  roleArn: "s_role_arn",
  credentialMode: "s_credential_mode",
} as const;
const PLAIN = {
  bucket: "bucket",
  region: "aws_region",
  roleArn: "aws_role_arn",
  credentialMode: "credential_mode",
} as const;

type ParamNames = typeof NAMESPACED | typeof PLAIN;

function parseCredentialMode(value: string | null): SourceCredentials["kind"] | null {
  return value === "ambient" || value === "literal" || value === "role" ? value : null;
}

function readSourceScope(
  params: URLSearchParams,
  names: ParamNames,
  fallback: SourceScope,
): SourceScope {
  const bucket = params.get(names.bucket) || fallback.bucket;
  const region = params.get(names.region) || fallback.region;
  const roleArn = params.get(names.roleArn) || "";
  const explicitMode = parseCredentialMode(params.get(names.credentialMode));

  if (explicitMode === "ambient") return makeSourceScope(bucket, region, AMBIENT_CREDENTIALS);
  if (explicitMode === "role") {
    return roleArn
      ? makeSourceScope(bucket, region, { kind: "role", roleArn })
      : makeSourceScope(bucket, region, AMBIENT_CREDENTIALS);
  }
  if (explicitMode === "literal") {
    return makeSourceScope(
      bucket,
      region,
      fallback.credentials.kind === "literal"
        ? fallback.credentials
        : EMPTY_LITERAL_CREDENTIALS,
    );
  }

  // Backward compatibility: legacy links represented role mode only by the ARN.
  if (roleArn) return makeSourceScope(bucket, region, { kind: "role", roleArn });
  // A pre-mode URL opened in the same tab as literal credentials keeps working.
  if (fallback.credentials.kind === "literal") {
    return makeSourceScope(bucket, region, fallback.credentials);
  }
  return makeSourceScope(bucket, region, AMBIENT_CREDENTIALS);
}

export function readNamespacedSourceScope(
  params: URLSearchParams,
  fallback: SourceScope = EMPTY_SOURCE_SCOPE,
): SourceScope {
  return readSourceScope(params, NAMESPACED, fallback);
}

export function readPlainSourceScope(
  params: URLSearchParams,
  fallback: SourceScope = EMPTY_SOURCE_SCOPE,
): SourceScope {
  return readSourceScope(params, PLAIN, fallback);
}

function writeUrlSourceScope(
  params: URLSearchParams,
  scope: UrlSourceScope,
  names: ParamNames,
): void {
  if (scope.bucket) params.set(names.bucket, scope.bucket);
  if (scope.region) params.set(names.region, scope.region);
  params.set(names.credentialMode, scope.credentials.kind);
  if (scope.credentials.kind === "role") {
    params.set(names.roleArn, scope.credentials.roleArn);
  }
}

/** Write safe browser/address-bar source state, including literal mode marker. */
export function writeUrlParams(params: URLSearchParams, scope: SourceScope): void {
  writeUrlSourceScope(params, toUrlSourceScope(scope), PLAIN);
}

/** Write safe namespaced browser scope state. */
export function writeNamespacedUrlParams(params: URLSearchParams, scope: SourceScope): void {
  writeUrlSourceScope(params, toUrlSourceScope(scope), NAMESPACED);
}

/** Share-only writer: the type excludes literal credentials. */
export function writeShareableParams(
  params: URLSearchParams,
  scope: ShareableSourceScope,
): void {
  writeUrlSourceScope(params, scope, PLAIN);
}

/** Namespaced share-only writer: the type excludes literal credentials. */
export function writeNamespacedParams(
  params: URLSearchParams,
  scope: ShareableSourceScope,
): void {
  writeUrlSourceScope(params, scope, NAMESPACED);
}

/**
 * Write an `/api/*` request URL. Credential mode and role are header-only;
 * region remains in the query for ambient cross-region reads.
 */
export function writeRequestParams(params: URLSearchParams, scope: SourceScope): void {
  if (scope.bucket) params.set(PLAIN.bucket, scope.bucket);
  if (scope.region) params.set(PLAIN.region, scope.region);
}

/** Build the existing backend credential headers from one canonical scope. */
export function credentialHeadersForSource(scope: SourceScope): Record<string, string> {
  const h: Record<string, string> = {};
  switch (scope.credentials.kind) {
    case "ambient":
      return h;
    case "literal":
      if (!isLiteralConfigured(scope.credentials)) return h;
      h["x-dial9-aws-access-key-id"] = scope.credentials.accessKeyId;
      h["x-dial9-aws-secret-access-key"] = scope.credentials.secretAccessKey;
      if (scope.credentials.sessionToken) {
        h["x-dial9-aws-session-token"] = scope.credentials.sessionToken;
      }
      break;
    case "role":
      h["x-dial9-aws-role-arn"] = scope.credentials.roleArn;
      break;
  }
  if (scope.region) h["x-dial9-aws-region"] = scope.region;
  return h;
}

/** Internal store operations needed to activate source state at page boot. */
export interface SourceScopeCredentials {
  get(): StoredSourceCredentials;
  setAmbient(): unknown;
  setLiteralMode(): unknown;
  setRoleArn(roleArn: string, opts?: { region?: string }): unknown;
  setRegion(region: string): unknown;
}

/** Make the credential store's active transport match an incoming source URL. */
export function applyToCreds(scope: SourceScope, creds: SourceScopeCredentials): void {
  switch (scope.credentials.kind) {
    case "ambient":
      creds.setAmbient();
      return;
    case "literal": {
      const stored = creds.get();
      if (stored.kind !== "literal") creds.setLiteralMode();
      if (scope.region && stored.region !== scope.region) creds.setRegion(scope.region);
      return;
    }
    case "role":
      creds.setRoleArn(
        scope.credentials.roleArn,
        scope.region ? { region: scope.region } : undefined,
      );
  }
}
