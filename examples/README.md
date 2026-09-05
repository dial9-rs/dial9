## Dial9 Examples
This directory contains five larger examples:
1. `metrics-service`: a service that collects metrics and publishes them to DynamoDb
2. `simple-local`: a minimal example showing how to wire up an application
3. `memory-local`: a memory profiling example.
4. `rayon-fanout`: generates a trace whose CPU samples fan out through rayon's
   work-stealing scheduler, producing a flamegraph with very high path
   cardinality — a fixture for viewer payload-size and readability work.
5. `telemetry-test-app`: a self-describing CPU, task-dump, and span integration fixture.

There are a host of other examples in [`dial9/examples/`](../dial9/examples) that demonstrate other patterns of usage.
