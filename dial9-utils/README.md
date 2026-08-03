# dial9-utils

[![Crates.io](https://img.shields.io/crates/v/dial9-utils.svg)](https://crates.io/crates/dial9-utils)
![License](https://img.shields.io/crates/l/dial9-utils.svg)

Opt-in integrations for [dial9](https://crates.io/crates/dial9).

The `tracing-layer` feature provides `Dial9TracingLayer`, a
`tracing_subscriber` layer that records span enter/exit events. Span events
carry the current Tokio task ID when one is available, without depending on
the dial9 Tokio runtime integration.

`dial9_axum` provides traced replacements for `axum::serve`, spawning connection
and HTTP/2 tasks through a dial9 executor so per-connection work lands in the
trace.

The tracing layer is also re-exported as `dial9::tracing_layer`. Add this crate
directly when using an integration such as Axum that is not re-exported by the
facade:

```toml
dial9 = { version = "0.5", features = ["tokio"] }
dial9-utils = { version = "0.5", features = ["axum-08"] }
```

## License

This project is licensed under the Apache-2.0 License.
