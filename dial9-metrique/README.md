# dial9-metrique

[![Crates.io](https://img.shields.io/crates/v/dial9-metrique.svg)](https://crates.io/crates/dial9-metrique)
![License](https://img.shields.io/crates/l/dial9-metrique.svg)

The [metrique](https://docs.rs/metrique) sink for [dial9](https://crates.io/crates/dial9): records unit-of-work metric entries into the dial9 trace as a peer of your existing EMF/JSON pipeline, so per-request application metrics land on the same timeline as runtime telemetry.

Most users want the [`dial9`](https://crates.io/crates/dial9) crate with the `metrique-sink` feature, which re-exports this crate as `dial9::metrique_sink` (add dial9's `tokio` feature to capture task ids). This crate itself is tokio-free and can be used directly by applications that do not run on tokio.

An entry opts in by including a `Dial9Context`, and `Dial9Stream::tee` wires dial9 alongside the pipeline you already have:

```rust,ignore
use dial9::metrique_sink::{Dial9Context, Dial9Stream};
use metrique::unit_of_work::metrics;

#[metrics(rename_all = "PascalCase")]
struct RequestMetrics {
    // Including a Dial9Context opts this entry into the trace.
    #[metrics(flatten)]
    dial9: Dial9Context,
    #[metrics(flags(dial9::Interned, dial9::SpanName))]
    operation: &'static str,
    latency_ms: u64,
}

// `dial9.*` context fields stay out of the EMF output.
let _join = ServiceMetrics::attach_to_stream(Dial9Stream::tee(&handle, emf_stream));

let mut m = RequestMetrics {
    dial9: Dial9Context::capture(),
    operation: "GetPet",
    latency_ms: 0,
}
.append_on_drop(ServiceMetrics::sink());
```

For entries you cannot (or would rather not) add a field to, `append_on_drop_dial9` attaches the same context from the outside:

```rust,ignore
use dial9::metrique_sink::Dial9EntryExt;

let mut m = RequestMetrics { operation: "GetPet", latency_ms: 0 }
    .append_on_drop_dial9(ServiceMetrics::sink());
```

See [docs.rs/dial9-metrique](https://docs.rs/dial9-metrique) for the full opt-in model, overhead numbers, and limitations, and the [repository](https://github.com/dial9-rs/dial9) for the dial9 guide.

## License

This project is licensed under the Apache-2.0 License.
