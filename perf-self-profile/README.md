# dial9-perf-self-profile

The self-profiling sources behind [dial9](https://crates.io/crates/dial9): CPU sampling, kernel scheduler events, heap allocation profiles, process resource usage, socket accept queues, and NVIDIA GPU metrics.

CPU sampling uses Linux `perf_event_open` where available and falls back to a signal-timer sampler when perf is restricted. The other sources are independent and don't use perf.

Most users want the [`dial9`](https://crates.io/crates/dial9) crate, which wraps these behind a builder (`.with_cpu_profiling(..)`, `.with_memory_profiling(..)`, and friends) and records them into a trace.

## CUDA GPU profiling

Enable the `cuda` feature to sample an NVIDIA GPU visible through NVML:

```rust
use dial9_core::buffer::MemoryBuffer;
use dial9_core::recorder::recorder;
use dial9_perf_self_profile::{CudaGpuConfig, RecorderPerfExt};

let writer = MemoryBuffer::new(64 * 1024)?;
let recorder = recorder(writer)
    .with_cuda_gpu_profiling(CudaGpuConfig::default())
    .build();
# Ok::<(), Box<dyn std::error::Error>>(())
```

To sample a specific GPU, select its NVML device index with
`CudaGpuConfig::builder().device_index(index).build()`.

Each sample includes GPU compute and memory-I/O utilization, used and total
framebuffer memory, and PCIe throughput when the device supports it. PCIe
throughput is aggregate device traffic, not traffic attributed to this process.
The NVIDIA driver library is loaded dynamically; when it or an NVIDIA GPU is
absent, the convenience method leaves GPU profiling disabled.

See [docs.rs/dial9-perf-self-profile](https://docs.rs/dial9-perf-self-profile) for the standalone API and the [repository](https://github.com/dial9-rs/dial9) for the full guide.

## License

Licensed under either of Apache-2.0 or MIT at your option.
