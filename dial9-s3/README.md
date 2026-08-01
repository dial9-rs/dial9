# dial9-s3

The S3 upload destination for [dial9](https://crates.io/crates/dial9): a pipeline
stage that uploads sealed trace segments to S3.

Enable it through the facade with the `worker-s3` feature rather than depending
on this crate directly:

```toml
dial9 = { version = "0.5", features = ["worker-s3"] }
```
