//! Individual allocation calls through process-global Dial9-wrapped jemalloc.

#[cfg(unix)]
mod allocator_calls;

#[cfg(unix)]
#[global_allocator]
static ALLOC: dial9_tokio_telemetry::memory_profiling::Dial9Allocator<tikv_jemallocator::Jemalloc> =
    dial9_tokio_telemetry::memory_profiling::Dial9Allocator::new(tikv_jemallocator::Jemalloc);

#[cfg(unix)]
fn main() {
    allocator_calls::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("The jemalloc call benchmark requires Unix.");
}
