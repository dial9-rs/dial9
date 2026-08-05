# dial9-utils

[![Crates.io](https://img.shields.io/crates/v/dial9-utils.svg)](https://crates.io/crates/dial9-utils)
![License](https://img.shields.io/crates/l/dial9-utils.svg)

Opt-in integrations for [dial9](https://crates.io/crates/dial9).

The `tracing-layer` feature provides `Dial9TracingLayer`, a
`tracing_subscriber` layer that records span enter/exit events. Span events
carry the current Tokio task ID when one is available; `dial9-utils` obtains
that ID directly from Tokio rather than depending on `dial9-tokio-telemetry`.

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

**Enable the `tracing-layer` feature:**
```toml
[dependencies]
dial9-utils = { version = "0.5", features = ["tracing-layer"] }
```

**Use tracing_subscriber to connect the `Dial9TracingLayer`:**

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

To opt in individual spans instead of entire targets, mark them with a
`dial9 = true` field and filter the layer by the presence of that field:

```rust
use dial9_utils::tracing_layer::Dial9TracingLayer;
use tracing_subscriber::prelude::*;

let dial9_spans = tracing_subscriber::filter::filter_fn(|metadata| {
    metadata.is_span() && metadata.fields().field("dial9").is_some()
});

tracing_subscriber::registry()
    .with(Dial9TracingLayer::new().with_filter(dial9_spans))
    .init();

let span = tracing::info_span!("handle_request", dial9 = true);
let _entered = span.enter();
```

The field is a marker: the filter selects spans that declare `dial9`, so use
`dial9 = true` consistently rather than expecting its value to be inspected.

## License

This project is licensed under the Apache-2.0 License.
