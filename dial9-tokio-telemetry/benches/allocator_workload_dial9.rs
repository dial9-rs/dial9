//! Application workloads using Dial9-wrapped jemalloc.

#[cfg(unix)]
mod allocator_workload;

#[cfg(unix)]
#[global_allocator]
static ALLOC: dial9_tokio_telemetry::memory_profiling::Dial9Allocator<tikv_jemallocator::Jemalloc> =
    dial9_tokio_telemetry::memory_profiling::Dial9Allocator::new(tikv_jemallocator::Jemalloc);

#[cfg(unix)]
fn main() {
    allocator_workload::run("dial9_jemalloc", true);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("The jemalloc workload experiment requires Unix.");
}
