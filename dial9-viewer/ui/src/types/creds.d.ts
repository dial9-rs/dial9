// Type declarations for the frozen-core file `creds.js`.
//
// The module export is dial9's full internal credential store. The browser
// global `window.Dial9Creds` is deliberately narrower and is declared by the
// page globals that consume the userscript contract.

declare module "*/creds.js" {
  export interface AmbientCredentials {
    kind: "ambient";
  }

  export interface LiteralCredentials {
    kind: "literal";
    accessKeyId: string;
    secretAccessKey: string;
    sessionToken?: string | undefined;
    /** Persistence compatibility only; canonical SourceScope owns region. */
    region?: string | undefined;
  }

  export interface RoleCredentials {
    kind: "role";
    roleArn: string;
    /** Persistence compatibility only; canonical SourceScope owns region. */
    region?: string | undefined;
  }

  export type StoredCredentials =
    | AmbientCredentials
    | LiteralCredentials
    | RoleCredentials;

  export interface BucketInfo {
    name: string;
    region: string | null;
  }

  export interface SetCredentialsInput {
    accessKeyId: string;
    secretAccessKey: string;
    sessionToken?: string;
    region?: string;
    autoDetectRegion?: boolean;
    bucket?: string;
  }

  export interface CredentialCheckResult {
    ok: boolean;
    region?: string | null;
    error: string | null;
  }

  export interface CredentialStorage {
    getItem(key: string): string | null;
    setItem(key: string, value: string): void;
    removeItem(key: string): void;
  }

  /** Exact public facade used by the private Tampermonkey integration. */
  export interface Dial9CredsUserscriptApi {
    set(creds: SetCredentialsInput): Promise<CredentialCheckResult>;
    check(bucket?: string): Promise<CredentialCheckResult>;
    setRegion(region: string): StoredCredentials;
  }

  /** Full internal store imported by shipped page modules. */
  export interface Dial9CredsApi extends Dial9CredsUserscriptApi {
    get(): StoredCredentials;
    has(): boolean;
    setAmbient(): AmbientCredentials;
    setLiteralMode(): LiteralCredentials;
    setRoleArn(
      roleArn: string,
      opts?: { region?: string | undefined },
    ): RoleCredentials;
    isValidRoleArn(arn: string): boolean;
    parse(text: string): {
      accessKeyId: string;
      secretAccessKey: string;
      sessionToken?: string;
      region?: string;
    };
    listBuckets(): Promise<BucketInfo[]>;
    clear(): AmbientCredentials;
    headers(): Record<string, string>;
    _setStorage(storage: CredentialStorage): void;
  }

  export const Dial9Creds: Dial9CredsApi;
}
