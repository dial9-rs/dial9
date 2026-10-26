# Allocator benchmarks

`memory_profiling_bench` compares direct `GlobalAlloc` calls on plain and
Dial9-wrapped System and jemalloc. Its original wrapped-System names are
preserved. `BENCH_CONFIG=none`, `sampling_only`, and `sampling_with_liveset`
select profiler state; an unknown value is an error.

The two application binaries compile the same workloads with different
`#[global_allocator]` declarations. `allocator_workload_plain` installs plain
jemalloc; `allocator_workload_dial9` installs `Dial9Allocator<Jemalloc>`. There
is no runtime allocator selection branch.

The four `allocator_calls_{jemalloc,system}_{plain,dial9}` binaries measure
individual global `alloc`, `alloc_zeroed`, `dealloc`, and `realloc` calls.
They time batches of up to 1,024 calls and divide by the number of calls,
using Criterion's `iter_custom`. Allocation setup, content checks, and
cleanup stay outside timing. Allocation sizes are 64 B; realloc grows
64 B to 128 B with initialized contents. This produces per-operation
measurements instead of dividing an alloc/free pair's time in half.

Build the four per-call executables:

```sh
cargo bench -p dial9-tokio-telemetry --features memory-profiling \
  --bench allocator_calls_jemalloc_plain --bench allocator_calls_jemalloc_dial9 \
  --bench allocator_calls_system_plain --bench allocator_calls_system_dial9 \
  --no-run --locked
```

Copy the executable paths printed by Cargo outside `target`, then pass them
as `--binary LABEL=PATH`:

```sh
python3 scripts/allocator-hook-experiment.py --kind micro \
  --binary plain=/tmp/jemalloc-plain --binary dial9=/tmp/jemalloc-dial9 \
  --config none --config sampling_only --config sampling_with_liveset \
  --rounds 3 --seed 77 --output /tmp/allocator-calls
```

Repeat for System. The plain executables accept active configurations and
install the same recorder/profiler setup, but their allocator never invokes
Dial9's hooks. For total cost over a process without profiling, subtract the
plain `none` baseline from each wrapped configuration. Comparing matching
configurations also controls for profiler setup and allocator state.
Sampling uses a 512 KiB mean interval and seed 42; active costs average sample
hits and misses. Each configuration runs in a fresh process because profiler
installation is permanent.

The `events` workload parses 300,000 JSON events into owned strings and vectors,
moves batches through a bounded Tokio channel, aggregates by route, and
serializes the result. The fixtures are deterministic synthetic events, not
a customer's captured pipeline. It uses a current-thread runtime so a pinned
CPU experiment measures allocator overhead without scheduler migration noise.
Input generation and runtime construction happen before timing.

The `trace` workload replays the captured, bundled demo trace through the
production Rust trace decoder and deserializes each event into an owned
`serde_json::Value`. File reading and gzip decompression happen before timing.
This measures decode/materialization, not the complete viewer or its Parquet
aggregation. Use `--trace PATH` to supply a different raw or gzip trace.

Both workloads run one untimed warmup, then three timed iterations by default.
Every iteration verifies the same event count or aggregate checksum. Results
are emitted as JSON, including all timings. `--iterations N` and `--events N`
override the defaults. Recording is off unless `BENCH_CONFIG` requests sampling
in the Dial9 binary.

Build the application binaries and existing alloc/free microbenchmark:

```sh
cargo bench -p dial9-tokio-telemetry --features memory-profiling \
  --bench memory_profiling_bench --bench allocator_workload_plain \
  --bench allocator_workload_dial9 --no-run --locked
```

To compare revisions, save each executable outside `target` and rebuild with
the same toolchain, flags, profile, dependencies, and harness. For example:

```sh
python3 scripts/allocator-hook-experiment.py --kind micro \
  --binary baseline=/tmp/baseline-micro --binary current=/tmp/current-micro \
  --config none --rounds 9 --filter tight_alloc \
  --output /tmp/allocator-tight

python3 scripts/allocator-hook-experiment.py --kind workload \
  --binary baseline=/tmp/baseline-workload --binary current=/tmp/current-workload \
  --config none --config sampling_only --config sampling_with_liveset \
  --rounds 9 --seed 77 \
  --output /tmp/allocator-workloads
```

The runner pins each invocation to CPU 2 by default, runs serially, and
randomizes variant/workload order within every round with seed 42. It retains
logs and raw measurements after every invocation and prints median timings.
Keep generated output outside the repository. `--cpu` changes affinity.
For application comparisons, use plain jemalloc with `--config none`; active
configurations require wrapped binaries.

`allocator_probe_dealloc` is a retained symbol forwarding to the actual global
deallocation path. Inspect it in each saved workload binary with:

```sh
objdump -d --disassemble=allocator_probe_dealloc /tmp/current-workload
```

## Measurements, October 23, 2026

Shared x86_64 KVM host, AMD EPYC 9R14, Rust 1.98 / LLVM 22.1.8, CPU 2,
release profile with frame pointers and no LTO. The
[memory-profiling README](../../../dial9/README.md#performance) lists added
nanoseconds per call from three randomized rounds. Its plain-allocator
baselines, with no profiler installed, were:

| Operation | jemalloc ns/call | System ns/call |
| --- | ---: | ---: |
| `alloc`, 64 B | 6.97 | 5.96 |
| `alloc_zeroed`, 64 B | 10.99 | 11.97 |
| `dealloc`, 64 B | 7.02 | 6.65 |
| `realloc`, 64 → 128 B | 28.72 | 26.06 |

Application results compare allocator/hook source from
`bdf248c80da931a86599f91f5b45b7c1df97072f` (#1065) against the fast-path
implementation, using identical benchmark code and security-patched
dependencies. Values are medians of process medians from nine randomized
rounds (seed 77), with one warmup and three timed iterations per process.
Counts and checksums matched.

| Configuration | Workload | Baseline ms | Fast path ms | Change |
| --- | --- | ---: | ---: | ---: |
| No profiler | Captured trace decoder | 101.272 | 75.722 | −25.23% |
| No profiler | Synthetic 300k events | 161.915 | 162.645 | +0.45% |
| Sampling only | Captured trace decoder | 111.884 | 80.176 | −28.34% |
| Sampling only | Synthetic 300k events | 172.862 | 168.085 | −2.76% |
| Sampling + liveset | Captured trace decoder | 129.486 | 98.295 | −24.09% |
| Sampling + liveset | Synthetic 300k events | 205.530 | 202.516 | −1.47% |

Plain jemalloc with no profiler measured 93.157 ms for the decoder and
159.606 ms for synthetic events; the wrapper measured −18.72% and +1.90%
respectively. Decoder gains depend on generated code/layout, with their
precise cause unresolved. Signed differences near zero include measurement
noise and allocator state. These workloads do not reproduce the unavailable
customer replay or establish a fleet-wide overhead bound.
