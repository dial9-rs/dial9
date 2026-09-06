use std::time::Duration;

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use dial9::{DiskBuffer, recorder};
use dial9_perf_self_profile::{cuda::CudaGpuConfig, RecorderPerfExt};

const TRACE_DIR: &str = "traces";

fn cos_sin(n: usize, device: &Device) -> Result<Tensor> {
    let thetas: Vec<_> = (0..n).map(|i| i as f32 / n as f32).collect();
    let xs: Vec<_> = thetas.iter().map(|t| t.cos().abs()).collect();
    let ys: Vec<_> = thetas.iter().map(|t| t.sin().abs()).collect();
    let xs = Tensor::from_vec(xs, (n, 1), device)?;
    let ys = Tensor::from_vec(ys, (1, n), device)?;
    let ys = Tensor::cat(&[&ys, &ys, &ys, &ys, &ys, &ys], 1)?;
    Ok(xs.matmul(&ys)?)
}

fn main() -> Result<()> {
    let writer = DiskBuffer::builder()
        .base_path(TRACE_DIR)
        .max_file_size(10 * 1024 * 1024)
        .max_total_size(50 * 1024 * 1024)
        .build()?;
    let recorder = recorder(writer)
        .try_with_cuda_gpu_profiling(
            CudaGpuConfig::builder()
                .sample_interval(Duration::from_millis(100))
                .build(),
        )
        .context("failed to start NVIDIA GPU profiling")?
        .build();

    let device = Device::new_cuda(0)?;
    let n = std::env::args()
        .nth(1)
        .map(|value| value.parse())
        .transpose()
        .context("matrix size must be a positive integer")?
        .unwrap_or(2000usize);
    let xys_cpu = cos_sin(n, &Device::Cpu)?;
    let xys = cos_sin(n, &device)?;
    let sum_keepdim_cpu = xys_cpu.sum_keepdim(1)?;
    let sum_keepdim = xys.sum_keepdim(1)?;
    std::hint::black_box((&sum_keepdim_cpu, &sum_keepdim));

    let start = std::time::Instant::now();
    let n_iters = 100;
    let mut v = 0f32;
    for _ in 0..n_iters {
        let sum_keepdim = xys.sum_keepdim(1)?;
        let sum_keepdim = sum_keepdim.sum_keepdim(0)?;
        let sum_keepdim: f32 = sum_keepdim.reshape(&[])?.to_scalar()?;
        v += sum_keepdim;
    }
    let elapsed = start.elapsed();
    if v > 0. {
        println!(
            "ran {n_iters} iterations, time per iter: {:?} ({v})",
            elapsed.div_f64(n_iters as f64)
        );
    }

    // Let the flush thread capture a final sample, then seal the trace file.
    std::thread::sleep(Duration::from_millis(200));
    recorder.graceful_shutdown(Duration::from_secs(5));

    println!("Trace written to {TRACE_DIR}/trace.0.bin");
    println!("View it with a viewer built from the same dial9 revision:");
    println!(
        "cargo run --manifest-path ../dial9/Cargo.toml -p dial9-viewer -- serve --local-dir {TRACE_DIR}"
    );
    Ok(())
}
