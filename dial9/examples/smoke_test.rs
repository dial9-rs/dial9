//! Example: check dial9 telemetry, as a startup probe would.
//!
//! ```sh
//! cargo run --example smoke_test
//! ```

use dial9::MemoryBuffer;
use dial9::smoke_test::SmokeTesterExt;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let recorder = dial9::recorder(MemoryBuffer::new(1 << 20)?).build();
    let tester = recorder.handle().smoke_tester().build();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let report = runtime.block_on(tester.run());
    print!("{report}");

    recorder.graceful_shutdown(Duration::from_secs(1));
    if !report.is_healthy() {
        std::process::exit(1);
    }
    Ok(())
}
