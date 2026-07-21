// Typed seam over the frozen creds.js bring-your-own-credentials store.
// Trace fetches carry its x-dial9-aws-* headers, which is why the seam
// lives in lib/trace. Importing it also runs creds.js's side effect of
// publishing `window.Dial9Creds` (the stable userscript contract), so a
// bundled page behaves like the legacy <script src="creds.js"> pages.

export { Dial9Creds } from "../../../creds.js";
export type {
  CredentialCheckResult,
  Dial9CredsApi,
  SetCredentialsInput,
  StoredCredentials,
} from "../../../creds.js";
