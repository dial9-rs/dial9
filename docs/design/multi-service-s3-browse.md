# Multi-Service S3 Browse Work

Taskei: `V2297602741` - Improve S3 browse view to support multi-service.

## Workspace

- Changes are on `feat/multi-service-s3-browse`.
- The branch was created in a new worktree from `origin/main`:
  `/workplace/rcoh/dial9-userscript/src/dial9-multi-service`.
- The original `~/code/dial9` checkout was not modified.
- No userscript-only change was made because the existing backend could not
  list a single service efficiently or exactly.

## Implemented

- Added an optional exact `service` parameter to `GET /api/browse`.
- Added bounded `GET /api/services` discovery over the selected time window.
  It returns sorted service names and unique host counts without returning
  browse objects.
- Service searches list the existing
  `{prefix}/{date}/{HHMM}/{service}/` key space and exclude sibling services.
- Service values containing `/` or control characters return HTTP 400.
- Local known-layout listings also support exact service filtering.
- Both browser UIs run service discovery when a bucket becomes ready.
- A sole service is focused and loaded automatically. Multiple services render
  as tabs and do not load browse data until a tab is activated.
- Added `service` to shareable URL state and viewer scope resolution.
- Service tab changes push History entries; deep links and Back/Forward restore
  the focused tab.
- Flamegraph, Tokio Stats, and Spans links use the active service's loaded time
  range when there is no explicit heatmap selection.
- On-demand refinement and folded-marker discovery use service-qualified
  prefixes, excluding sibling services at S3 LIST time.
- Stale discovery and browse responses are ignored when users switch quickly.
- A pending service-scoped link starts once credential setup supplies its
  bucket.
- Updated UI documentation and the legacy/new switch round-trip check.

## Verification

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- Full `cargo nextest run`: 1,099 passed
- Two stress iterations: 1,099 passed each
- Full Vitest suite: 1,829 passed, 1 expected failure, 12 skipped
- Full legacy Node test suite passed
- TypeScript, Vite build, inline-script parsing, import-boundary, and
  `git diff --check` passed

## Deferred Work

The default writer layout was not changed to put service before date. Existing
readers assume date-first keys and could silently misparse a service-first key.
Service-first also needs a strategy for discovery and unfiltered browsing.

The current date-first layout requires one delimiter listing per minute for
discovery and one exact listing per minute for a service-scoped search (61 of
each for a one-hour window). A future migration should introduce a versioned
`TraceKeyLayout`, deploy dual readers before new writers, and provide a durable
service catalog.
