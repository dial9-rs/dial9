# dial9-destinations-s3

The S3 upload destination for [dial9](https://crates.io/crates/dial9): a pipeline
stage that uploads sealed trace segments to S3.

Enable it through the facade with the `worker-s3` feature rather than depending
on this crate directly:

```toml
dial9 = { version = "0.5", features = ["worker-s3"] }
```

## Default object keys

The default uploader writes trace segments using a versioned Hive-style layout:

```text
{prefix}/version=1/date={YYYY-MM-DD}/service={service}/time={HHMM}/instance={instance}/boot={boot_id}/{epoch_secs}-{index}.bin[.gz]
```

Partition values use Hive path escaping. For example, `payments/api` becomes
`service=payments%2Fapi`; `%` and `=` become `%25` and `%3D`. The optional
prefix remains an opaque namespace and is not escaped by the uploader.

`S3Config::key_fn` can still replace the default layout completely. Custom
keys are stored verbatim and are not required to follow the Hive-style format;
automatic discovery in the S3 Browser applies only to supported layouts.
