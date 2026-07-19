//! Record metrique entries into the dial9 trace, composed alongside a
//! "real" metric sink via `tee`.
//!
//! Defines a small `RequestMetrics` struct, flattens `Dial9Context` into it,
//! and tees every entry to both `metrique::local`'s human-readable stdout
//! format (standing in for a production EMF/JSON sink) and `Dial9Stream`.
//! This is the intended composition — `Dial9Stream` is a peer sink, not a
//! replacement for your existing metrique pipeline.
//!
//! Usage:
//!   cargo run --example metrique_integration --features metrique-integration
//!
//! Inspect the trace afterwards:
//!   cargo run --example analyze_trace --features analysis -- metrique_integration_trace.0.bin

use dial9_tokio_telemetry::Dial9Config;
use dial9_tokio_telemetry::telemetry::Dial9Handle;
use dial9_tokio_telemetry::telemetry::metrique_integration::{Dial9Context, Dial9Stream, Emit};
use metrique::CloseValue;
use metrique::RootEntry;
use metrique::local::{LocalFormat, OutputStyle};
use metrique::unit_of_work::metrics;
use metrique::writer::EntryIoStream;
use metrique::writer::format::FormatExt;
use metrique::writer::stream::tee;

#[metrics(rename_all = "PascalCase", default_flags(Emit))]
struct RequestMetrics {
    #[metrics(flatten)]
    dial9: Dial9Context,
    operation: &'static str,
    status_code: u64,
}

async fn handle_request(operation: &'static str, status_code: u64) -> RequestMetrics {
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    RequestMetrics {
        operation,
        status_code,
        dial9: Dial9Context::capture(),
    }
}

#[dial9_tokio_telemetry::main(config = || {
    Dial9Config::builder()
        .on_disk_buffer("metrique_integration_trace.bin")
        .max_file_size(16 * 1024 * 1024)
        .max_total_size(64 * 1024 * 1024)
        .build_or_disabled()
})]
async fn main() {
    let handle = Dial9Handle::current();

    // `LocalFormat` stands in for a production EMF/JSON sink here — it
    // prints each entry to stdout in a human-readable form. `tee` fans the
    // same entry out to both it and `Dial9Stream`, unmodified: this is the
    // primitive composition described in `docs/design/metrique-integration.md`.
    let stdout_sink = LocalFormat::new(OutputStyle::Pretty).output_to(std::io::stdout());
    let mut stream = tee(stdout_sink, Dial9Stream::new(&handle));

    for (operation, status_code) in [("GetUser", 200), ("CreateOrder", 201), ("GetUser", 404)] {
        let metrics = handle_request(operation, status_code).await;
        let closed = CloseValue::close(metrics);
        let entry = RootEntry::new(closed);
        stream.next(&entry).expect("recording should not error");
    }

    println!("Trace written to metrique_integration_trace.*.bin");
}
