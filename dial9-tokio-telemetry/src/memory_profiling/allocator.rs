//! `Dial9Allocator<A>` — a `GlobalAlloc` wrapper that feeds dial9's memory
//! profiler.

use std::alloc::{GlobalAlloc, Layout};

/// Generic `GlobalAlloc` wrapper that feeds dial9's memory profiler.
///
/// This type is designed to be installed as the process-global allocator
/// via `#[global_allocator]`. Used in any other context (e.g. as a local
/// allocator in `Box::new_in`) it will not enable memory profiling because
/// the hook only intercepts allocations that route through the global
/// allocator slot.
///
/// Defaults to wrapping `std::alloc::System`. Use `Dial9Allocator::system()`
/// (no turbofish) for the common case, or `Dial9Allocator::new(inner)` to
/// wrap a custom allocator like `tikv_jemallocator::Jemalloc` or
/// `mimalloc::MiMalloc`.
///
/// # Examples
///
/// Wrap the system allocator:
///
/// ```no_run
/// use dial9_tokio_telemetry::memory_profiling::Dial9Allocator;
///
/// #[global_allocator]
/// static ALLOC: Dial9Allocator = Dial9Allocator::system();
/// # fn main() {}
/// ```
///
/// Wrap a custom allocator:
///
/// ```ignore
/// use dial9_tokio_telemetry::memory_profiling::Dial9Allocator;
///
/// #[global_allocator]
/// static ALLOC: Dial9Allocator<tikv_jemallocator::Jemalloc> =
///     Dial9Allocator::new(tikv_jemallocator::Jemalloc);
/// ```
///
/// # Cost
///
/// Until [`MemoryProfiler::install()`](super::MemoryProfiler::install) has
/// been called, this is a zero-cost passthrough (~1 ns Acquire load +
/// null check). After install, ~99.9% of allocations take the unsampled
/// fast path (~5 ns). The remaining ~0.1% of sampled allocations pay
/// ~1 µs for stack capture and event emission.
#[derive(Debug)]
pub struct Dial9Allocator<A = std::alloc::System>(A);

impl Dial9Allocator {
    /// Wrap the system allocator.
    ///
    /// Use this when you don't need a custom inner allocator (i.e. you
    /// weren't otherwise setting `#[global_allocator]`).
    pub const fn system() -> Self {
        Self(std::alloc::System)
    }
}

impl<A: GlobalAlloc> Dial9Allocator<A> {
    /// Wrap a custom inner allocator (e.g. jemalloc, mimalloc).
    pub const fn new(inner: A) -> Self {
        Self(inner)
    }
}

// SAFETY: forwarding to the inner `GlobalAlloc` impl. The `unsafe` contract on
// each method is exactly the same as the inner allocator's. The hook
// invocations are allocation-free by construction — see
// [`crate::memory_profiling::hook`] module docs and design §6.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Dial9Allocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` validity contract forwarded to the inner allocator.
        let ptr = unsafe { self.0.alloc(layout) };
        // SAFETY: `on_alloc` is allocation-free (see hook module docs) — it
        // performs no allocations, takes no locks, and only accesses lock-free
        // data structures. `ptr` is non-null (checked below) and valid.
        if !ptr.is_null()
            && let Some(inner) = crate::memory_profiling::profiler::ACTIVE.get()
        {
            crate::memory_profiling::hook::on_alloc(inner, ptr, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `on_dealloc` is allocation-free (see hook module docs) — it
        // performs no allocations, takes no locks, and only accesses lock-free
        // data structures. `ptr` is valid per the `dealloc` contract.
        if let Some(inner) = crate::memory_profiling::profiler::ACTIVE.get() {
            crate::memory_profiling::hook::on_dealloc(inner, ptr, layout.size());
        }
        // SAFETY: `ptr`/`layout` validity contract forwarded to the inner
        // allocator.
        unsafe { self.0.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded to the inner allocator. The hook below only
        // fires after we've confirmed the inner realloc returned non-null
        // — otherwise the old pointer is still live and must not be
        // recorded as freed (design §3, "realloc handling").
        let new_ptr = unsafe { self.0.realloc(ptr, old_layout, new_size) };
        // SAFETY: `on_realloc` is allocation-free (see hook module docs) — it
        // performs no allocations, takes no locks, and only accesses lock-free
        // data structures. `new_ptr` is non-null (checked below) and valid.
        if !new_ptr.is_null()
            && let Some(inner) = crate::memory_profiling::profiler::ACTIVE.get()
        {
            crate::memory_profiling::hook::on_realloc(
                inner,
                ptr,
                old_layout.size(),
                new_ptr,
                new_size,
            );
        }
        new_ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded to the inner allocator.
        let ptr = unsafe { self.0.alloc_zeroed(layout) };
        // SAFETY: `on_alloc` is allocation-free (see hook module docs) — it
        // performs no allocations, takes no locks, and only accesses lock-free
        // data structures. `ptr` is non-null (checked below) and valid.
        if !ptr.is_null()
            && let Some(inner) = crate::memory_profiling::profiler::ACTIVE.get()
        {
            crate::memory_profiling::hook::on_alloc(inner, ptr, layout.size());
        }
        ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout};

    #[test]
    fn dial9_allocator_default_constructible() {
        let _: Dial9Allocator = Dial9Allocator::system();
    }

    #[test]
    fn dial9_allocator_alloc_dealloc_roundtrip() {
        let allocator = Dial9Allocator::system();
        let layout = Layout::from_size_align(1024, 8).unwrap();
        // SAFETY: layout is valid; we own the returned pointer.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null(), "alloc returned null");
        // Touch the memory to make sure it's writable.
        unsafe { ptr.write_bytes(0x42, 1024) };
        // SAFETY: `ptr` was just allocated with this `layout`.
        unsafe { allocator.dealloc(ptr, layout) };
    }

    #[test]
    fn dial9_allocator_alloc_zeroed_returns_zero() {
        let allocator = Dial9Allocator::system();
        let layout = Layout::from_size_align(1024, 8).unwrap();
        // SAFETY: layout is valid.
        let ptr = unsafe { allocator.alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "alloc_zeroed returned null");
        let slice = unsafe { std::slice::from_raw_parts(ptr, 1024) };
        assert!(
            slice.iter().all(|&b| b == 0),
            "alloc_zeroed memory not zeroed"
        );
        // SAFETY: `ptr` was allocated with this `layout`.
        unsafe { allocator.dealloc(ptr, layout) };
    }

    #[test]
    fn dial9_allocator_realloc_grows() {
        let allocator = Dial9Allocator::system();
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: layout is valid.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null(), "initial alloc returned null");
        unsafe { ptr.write_bytes(0xAB, 64) };
        // SAFETY: ptr was allocated with `layout`, new_size (256) > 0.
        let new_ptr = unsafe { allocator.realloc(ptr, layout, 256) };
        assert!(!new_ptr.is_null(), "realloc returned null");
        // Verify original bytes preserved.
        let slice = unsafe { std::slice::from_raw_parts(new_ptr, 64) };
        assert!(
            slice.iter().all(|&b| b == 0xAB),
            "realloc didn't preserve data"
        );
        // Write to the grown region.
        unsafe { new_ptr.add(64).write_bytes(0xCD, 192) };
        // SAFETY: new_ptr was returned by realloc with new_size=256.
        let new_layout = Layout::from_size_align(256, 8).unwrap();
        unsafe { allocator.dealloc(new_ptr, new_layout) };
    }

    #[test]
    fn dial9_allocator_with_custom_inner_compiles() {
        let allocator = Dial9Allocator::new(std::alloc::System);
        let layout = Layout::from_size_align(128, 8).unwrap();
        // SAFETY: layout is valid.
        let ptr = unsafe { allocator.alloc(layout) };
        assert!(!ptr.is_null(), "alloc returned null");
        // SAFETY: ptr was allocated with this layout.
        unsafe { allocator.dealloc(ptr, layout) };
    }
}
