This standalone consumer runs the existing runtime integration tests against
Tokio 1.51.0. Its dependencies enable the production library features except
task dumps and the viewer CLI. A separate workspace keeps viewer-only S3 test
servers, which require newer Tokio, out of the consumer dependency graph.

The CI job resolves Tokio explicitly, prints the resolved version, and runs
nextest with `--locked`. Tokio 1.53.2 is tested against the full workspace,
including task dumps. Both cases are required by `CI Pass`.

Run from the repository root so the normal `.cargo/config.toml` flags apply:

```sh
cargo nextest run --manifest-path tests/min-versions/Cargo.toml --all-features
```
