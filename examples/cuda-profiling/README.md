# dial9 CUDA GPU profiling example

This example runs a small Candle workload on CUDA while dial9 samples GPU
utilization, framebuffer memory, and PCIe throughput through NVIDIA NVML. It
writes the resulting trace to `traces/trace.0.bin`.

Run it on a machine with an NVIDIA GPU and driver:

```sh
cargo run --release
```

An optional argument changes the matrix size (the default is `2000`):

```sh
cargo run --release -- 3000
```

Then open the trace in a viewer built from the sibling dial9 checkout. Using
the same revision matters because the GPU trace contains schema annotations:

```sh
cargo run --manifest-path ../dial9/Cargo.toml -p dial9-viewer -- \
  serve --local-dir "$PWD/traces"
```

Open <http://localhost:3000>, select `trace.0.bin`, and view the CUDA GPU
metrics. The CUDA toolkit is needed by Candle to run the workload; dial9 itself
loads NVML dynamically from the installed NVIDIA driver.

An existing trace is included to make it easier to see what is roughly expected of
the output.
