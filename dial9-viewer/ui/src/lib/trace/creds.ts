// lib/trace/creds.ts - typed seam over the frozen creds.js (T13;
// bring-your-own-credentials store). Trace fetches carry its
// x-dial9-aws-* headers (features/03 F8/F184), which is why the seam
// lives in lib/trace. Importing the module also runs its browser side
// effect of publishing `window.Dial9Creds` (the stable userscript
// contract) - the same global the legacy pages establish via
// <script src="creds.js">, so a bundled page behaves identically.

export { Dial9Creds } from "../../../creds.js";
export type {
  CredentialCheckResult,
  Dial9CredsApi,
  SetCredentialsInput,
  StoredCredentials,
} from "../../../creds.js";
