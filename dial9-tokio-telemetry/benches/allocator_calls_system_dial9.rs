//! Individual allocation calls through process-global Dial9-wrapped System.

mod allocator_calls;

#[global_allocator]
static ALLOC: dial9_tokio_telemetry::memory_profiling::Dial9Allocator =
    dial9_tokio_telemetry::memory_profiling::Dial9Allocator::system();

fn main() {
    allocator_calls::run();
}
