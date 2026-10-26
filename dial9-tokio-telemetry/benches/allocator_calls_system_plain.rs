//! Individual allocation calls through the process-global System allocator.

mod allocator_calls;

#[global_allocator]
static ALLOC: std::alloc::System = std::alloc::System;

fn main() {
    allocator_calls::run();
}
