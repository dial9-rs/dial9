# ADR-0005: Namespace the aggregate cache by AWS account

- **Status:** accepted
- **Date:** 2026-11-09

## Context

Aggregation folds source files into part-files in one shared output store,
and later requests reuse them (see [ADR-0003]). Requests sign source reads
with different credentials, and folded parts reveal their sources' contents,
so without a boundary a caller could read aggregates folded by a caller with
broader access. Checking access to every source object before reuse costs
one request per cached file per query.

## Decision

Scope the cache to a namespace chosen by the credentials that sign the
request's source reads:

```
{output_prefix}/account={namespace}/v{SAMPLES_FORMAT_VERSION}/bucket={source_bucket}/…
```

Explicit request credentials get a validated `{partition}.{account_id}`:

- **Assumed role:** from the validated role ARN, after STS assumes it.
- **Bring-your-own credentials:** from STS `GetCallerIdentity`, signed with
  those credentials in the request's validated region.

The server's ambient credential chain uses the fixed namespace `ambient`
and makes no `GetCallerIdentity` call, for both a configured S3 source and
a caller-selected bucket read in an ambient region. Sources without an AWS
identity use the fixed namespaces `local`, `simulator`, and `supplied`. If
the account of bring-your-own credentials cannot be resolved or STS returns
a malformed identity, the request fails with a generic message (401 only
when STS rejects the credentials, otherwise 503); there is no unscoped
fallback.

When output persists to S3, every request source backend is wrapped so
keys and listing prefixes at or under `{output_prefix}/account=…` or the
legacy `{output_prefix}/v{N}` answer as not found, in every bucket (an
access point alias can name the output bucket), after path normalization.
This covers `/api/object`, `/api/prefixes`, `/api/browse`, `/api/services`,
and aggregation's source reads. Other keys stay readable.

## Consequences

- **Account isolation applies to explicit credentials only.** Bring-your-own
  and assumed-role principals in one account share its cache, even with
  different source permissions, and never see another account's cache. Use
  separate output prefixes or stores for finer isolation.
- **Fixed namespaces are operational, not authorization boundaries.**
  `ambient`, `local`, `simulator`, and `supplied` are isolated from account
  namespaces, but servers using the same fixed name on a shared output
  store and prefix are not isolated from each other.
- A persistent cache read with ambient credentials needs one stable ambient
  identity per process. Processes with different ambient accounts must not
  share an output store and prefix; separate stores or prefixes isolate them.
- Bring-your-own credentials cost one STS call per aggregate request, so
  that path needs network access to STS.
- Output under the legacy `{output_prefix}/v{N}/` is never read; it can be
  deleted.
- The server's ambient credentials, the local directory, and the simulator
  each use their own fixed namespace (`ambient`, `local`, `simulator`).
  Every other backend, including any backend passed to `AppState::new`,
  shares the fixed `supplied` namespace. Servers whose supplied backends
  read with different credentials must not share an output store and
  prefix.
- Source keys under the protected subtrees are unreadable through source
  routes while S3 output is configured.

[ADR-0003]: 0003-folded-set-is-the-output-listing.md
