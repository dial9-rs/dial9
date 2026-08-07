#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
mod linux;
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub(crate) use linux::fp_profiler;
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub(crate) use linux::offline_symbolize::SymbolizeContainers;
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub(crate) use linux::offline_symbolize::write_symbol_data;
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub(crate) use linux::symbolize_one_shot;
#[cfg(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
))]
pub use linux::{
    PerfSampler, is_ctimer_active, resolve_symbol, resolve_symbol_with_maps,
    resolve_symbols_with_maps,
};

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
)))]
mod unsupported;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
)))]
pub(crate) use unsupported::symbolize_one_shot;
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "android", target_arch = "aarch64")
)))]
pub use unsupported::{PerfSampler, resolve_symbol};
