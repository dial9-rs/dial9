# dial9-perf-self-profile

The self-profiling sources behind [dial9](https://crates.io/crates/dial9): CPU sampling, kernel scheduler events, heap allocation profiles, process resource usage, and socket accept queues.

CPU sampling uses Linux `perf_event_open` where available and falls back to a signal-timer sampler when perf is restricted. The other sources are independent and don't use perf.

Most users want the [`dial9`](https://crates.io/crates/dial9) crate, which wraps these behind a builder (`.with_cpu_profiling(..)`, `.with_memory_profiling(..)`, and friends) and records them into a trace.

See [docs.rs/dial9-perf-self-profile](https://docs.rs/dial9-perf-self-profile) for the standalone API and the [repository](https://github.com/dial9-rs/dial9) for the full guide.

## License

Licensed under either of Apache-2.0 or MIT at your option.
