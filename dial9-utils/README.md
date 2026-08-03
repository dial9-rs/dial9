# dial9-utils

[![Crates.io](https://img.shields.io/crates/v/dial9-utils.svg)](https://crates.io/crates/dial9-utils)
![License](https://img.shields.io/crates/l/dial9-utils.svg)

Opt-in framework integrations for [dial9](https://crates.io/crates/dial9).

`dial9_axum` provides traced replacements for `axum::serve`, spawning connection
and HTTP/2 tasks through a dial9 executor so per-connection work lands in the
trace.

Unlike dial9's other sibling crates this one is not re-exported by the facade.
Add it alongside `dial9` and pick the feature for your Axum version (neither is
on by default):

```toml
dial9 = { version = "0.5", features = ["tokio"] }
dial9-utils = { version = "0.5", features = ["axum-08"] }
```

## License

This project is licensed under the Apache-2.0 License.
