//! Individual allocation calls through the process-global plain jemalloc.

#[cfg(unix)]
mod allocator_calls;

#[cfg(unix)]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(unix)]
fn main() {
    allocator_calls::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("The jemalloc call benchmark requires Unix.");
}
