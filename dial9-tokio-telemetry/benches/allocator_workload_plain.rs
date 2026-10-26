//! Application workloads using plain jemalloc. See allocator_workload/README.md.

#[cfg(unix)]
mod allocator_workload;

#[cfg(unix)]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(unix)]
fn main() {
    allocator_workload::run("jemalloc", false);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("The jemalloc workload experiment requires Unix.");
}
