use std::time::Duration;

use anyhow::{Context, Result};
use cudarc::cublas::{CudaBlas, Gemm, GemmConfig, Gemv, GemvConfig, sys::cublasOperation_t};
use cudarc::driver::CudaContext;
use dial9::{DiskBuffer, recorder};
use dial9_perf_self_profile::{RecorderPerfExt, cuda::CudaGpuConfig};

const TRACE_DIR: &str = "traces";

fn main() -> Result<()> {
    let n = std::env::args()
        .nth(1)
        .map(|value| value.parse())
        .transpose()
        .context("matrix size must be a positive integer")?
        .unwrap_or(2000usize);
    anyhow::ensure!(n > 0, "matrix size must be positive");
    let columns = n.checked_mul(6).context("matrix size is too large")?;
    let elements = n.checked_mul(columns).context("matrix size is too large")?;
    let rows_i32 = i32::try_from(n).context("matrix size exceeds cuBLAS limits")?;
    let columns_i32 = i32::try_from(columns).context("matrix size exceeds cuBLAS limits")?;

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

    let context = CudaContext::new(0)?;
    let stream = context.default_stream();
    let blas = CudaBlas::new(stream.clone())?;
    let xs: Vec<_> = (0..n).map(|i| (i as f32 / n as f32).cos().abs()).collect();
    let ys: Vec<_> = (0..columns)
        .map(|i| ((i % n) as f32 / n as f32).sin().abs())
        .collect();
    let expected = xs.iter().sum::<f32>() * ys.iter().sum::<f32>();
    let xs = stream.clone_htod(&xs)?;
    let ys = stream.clone_htod(&ys)?;
    let column_ones = stream.clone_htod(&vec![1f32; columns])?;
    let row_ones = stream.clone_htod(&vec![1f32; n])?;
    let mut xys = stream.alloc_zeros::<f32>(elements)?;
    let mut row_sums = stream.alloc_zeros::<f32>(n)?;
    let mut total = stream.alloc_zeros::<f32>(1)?;
    let no_transpose = cublasOperation_t::CUBLAS_OP_N;

    // SAFETY: column-major (n x 1) * (1 x 6n) writes exactly n * 6n elements.
    // Every buffer belongs to the same CUDA stream and remains alive.
    unsafe {
        blas.gemm(
            GemmConfig {
                transa: no_transpose,
                transb: no_transpose,
                m: rows_i32,
                n: columns_i32,
                k: 1,
                alpha: 1f32,
                lda: rows_i32,
                ldb: 1,
                beta: 0f32,
                ldc: rows_i32,
            },
            &xs,
            &ys,
            &mut xys,
        )?;
    }
    let sum_rows = GemvConfig {
        trans: no_transpose,
        m: rows_i32,
        n: columns_i32,
        alpha: 1f32,
        lda: rows_i32,
        incx: 1,
        beta: 0f32,
        incy: 1,
    };
    let sum_total = GemvConfig {
        trans: cublasOperation_t::CUBLAS_OP_T,
        m: rows_i32,
        n: 1,
        alpha: 1f32,
        lda: rows_i32,
        incx: 1,
        beta: 0f32,
        incy: 1,
    };
    stream.synchronize()?;

    let start = std::time::Instant::now();
    let n_iters = 100;
    let mut v = 0f32;
    for _ in 0..n_iters {
        // SAFETY: (n x 6n) * (6n) writes n row sums, then (n x 1)^T * (n)
        // writes one total. Buffers are disjoint and remain on the same stream.
        unsafe {
            blas.gemv(sum_rows, &xys, &column_ones, &mut row_sums)?;
            blas.gemv(sum_total, &row_sums, &row_ones, &mut total)?;
        }
        let actual = stream.clone_dtoh(&total)?[0];
        anyhow::ensure!(
            actual.is_finite() && (actual - expected).abs() <= expected.abs() * 1e-4,
            "GPU sum {actual} differs from CPU sum {expected}"
        );
        v += actual;
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
