# dial9-tokio-telemetry

[![Crates.io](https://img.shields.io/crates/v/dial9-tokio-telemetry.svg)](https://crates.io/crates/dial9-tokio-telemetry)
![License](https://img.shields.io/crates/l/dial9-tokio-telemetry.svg)

The Tokio integration internals for [dial9](https://crates.io/crates/dial9): `#[dial9::main]`, `TracedRuntime`, `dial9::spawn`, and the `recorder(..).with_tokio(..)` builder.

Use the [`dial9`](https://crates.io/crates/dial9) crate directly and enable its `tokio` feature, to add tokio instrumentation
capabilities to the dial9 recorder and access this crate's APIs.

See [docs.rs/dial9](https://docs.rs/dial9) and the [repository](https://github.com/dial9-rs/dial9) for setup and the full guide.

## License

This project is licensed under the Apache-2.0 License.
