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

Unlike dial9's other sibling crates this one is not re-exported by the facade.
Add it alongside `dial9` and enable only the integrations you need (none are on
by default):

```toml
dial9 = { version = "0.5", features = ["tokio"] }
dial9-utils = { version = "0.5", features = ["tracing-layer", "axum-08"] }
tracing = "0.1"
tracing-subscriber = "0.3"
```

## Tracing span events (opt-in)

Connect `Dial9TracingLayer` to a `tracing_subscriber` registry:

```rust
use dial9_utils::tracing_layer::Dial9TracingLayer;
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(
        Dial9TracingLayer::new().with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("my_app", tracing::Level::TRACE)
                .with_default(tracing::Level::ERROR),
        ),
    )
    .init();
```

Careful filtering is strongly recommended. Libraries like the AWS SDK emit
many internal spans that can produce over 100K events per second. The example
above captures only spans from `my_app`. Each span enter+exit costs roughly
650-800ns total on a modern server core, most of which is dial9 encoding (the
same span through a bare `tracing` registry costs roughly 100-200ns).

## License

This project is licensed under the Apache-2.0 License.
