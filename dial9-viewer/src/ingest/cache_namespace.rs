//! Cache namespaces for cached aggregate output.
//!
//! Folded part-files are derived from source objects, so reading them reveals
//! what those objects contain. Aggregate output is therefore partitioned by
//! namespace. A request's output keys, folded-set listing, and per-file fold
//! locks all live under `{output_prefix}/account={namespace}` (see
//! [`CacheNamespace::scope_output_prefix`]).
//!
//! Explicit bring-your-own and assumed-role credentials are namespaced by
//! their AWS partition and account, which isolates them from other accounts.
//! Principals in one account share that account's cache even when their IAM
//! policies grant different source objects.
//!
//! The fixed names below (`ambient`, `local`, `simulator`, `supplied`) are
//! operational namespaces, not authorization boundaries. They are isolated
//! from AWS account namespaces, not from each other across servers that share
//! an output store and prefix. The ambient chain is never resolved to an
//! account: a persistent cache read with ambient credentials needs one stable
//! ambient identity per process, and processes with different ambient
//! accounts need separate output stores or prefixes.

/// A validated cache namespace. Values are path-safe by construction, and AWS
/// values (`{partition}.{12 digits}`) never collide with the fixed names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheNamespace(String);

impl CacheNamespace {
    /// The server's ambient AWS credential chain.
    #[cfg(feature = "s3")]
    pub(crate) fn ambient() -> Self {
        Self("ambient".to_string())
    }

    /// A local-filesystem source.
    pub(crate) fn local() -> Self {
        Self("local".to_string())
    }

    /// The virtual simulator source.
    pub(crate) fn simulator() -> Self {
        Self("simulator".to_string())
    }

    /// A server-owned backend with no resolvable AWS identity: a supplied
    /// storage backend, or a backend an embedder passed to `AppState::new`.
    pub(crate) fn supplied() -> Self {
        Self("supplied".to_string())
    }

    /// An AWS account, or `None` when `partition` or `account_id` is malformed.
    #[cfg(feature = "s3")]
    pub(crate) fn aws_account(partition: &str, account_id: &str) -> Option<Self> {
        (is_valid_partition(partition) && is_valid_account_id(account_id))
            .then(|| Self(format!("{partition}.{account_id}")))
    }

    /// The AWS account of a `GetCallerIdentity` response. The partition comes
    /// from `arn`, whose account must match `account_id`; `None` on any
    /// malformed or inconsistent value.
    #[cfg(feature = "s3")]
    pub(crate) fn from_caller_identity(account_id: &str, arn: &str) -> Option<Self> {
        let [prefix, partition, service, region, arn_account, resource] =
            arn.splitn(6, ':').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let well_formed = prefix == "arn"
            && matches!(service, "iam" | "sts")
            && region.is_empty()
            && arn_account == account_id
            && !resource.is_empty();
        if !well_formed {
            return None;
        }
        Self::aws_account(partition, account_id)
    }

    #[cfg(all(test, feature = "s3"))]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Scope an aggregate output prefix to this namespace:
    /// `{output_prefix}/account={namespace}`. Output written before namespacing
    /// sits directly under `{output_prefix}/v{N}/` and is never read.
    pub(crate) fn scope_output_prefix(&self, output_prefix: &str) -> String {
        let base = output_prefix.trim_end_matches('/');
        if base.is_empty() {
            format!("account={}", self.0)
        } else {
            format!("{base}/account={}", self.0)
        }
    }
}

/// `aws` or `aws-` followed by hyphen-separated lowercase alphanumeric runs
/// (`aws-cn`, `aws-us-gov`, `aws-iso-b`, ...), at most 32 bytes.
#[cfg(feature = "s3")]
fn is_valid_partition(partition: &str) -> bool {
    if partition.len() > 32 {
        return false;
    }
    let Some(rest) = partition.strip_prefix("aws") else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    rest.split('-').all(|run| {
        !run.is_empty()
            && run
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

#[cfg(feature = "s3")]
fn is_valid_account_id(account_id: &str) -> bool {
    account_id.len() == 12 && account_id.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::named("flamegraph-data", "flamegraph-data/account=local")]
    #[case::trailing_slash("flamegraph-data/", "flamegraph-data/account=local")]
    #[case::empty("", "account=local")]
    fn scope_output_prefix_appends_one_segment(#[case] base: &str, #[case] want: &str) {
        assert_eq!(CacheNamespace::local().scope_output_prefix(base), want);
    }

    #[cfg(feature = "s3")]
    #[rstest::rstest]
    #[case::commercial("aws", "123456789012", Some("aws.123456789012"))]
    #[case::gov("aws-us-gov", "123456789012", Some("aws-us-gov.123456789012"))]
    #[case::short_account("aws", "12345", None)]
    #[case::unknown_partition("evil", "123456789012", None)]
    #[case::path_in_partition("aws-../x", "123456789012", None)]
    #[case::trailing_hyphen("aws-", "123456789012", None)]
    fn aws_account_validates(
        #[case] partition: &str,
        #[case] account: &str,
        #[case] want: Option<&str>,
    ) {
        assert_eq!(
            CacheNamespace::aws_account(partition, account)
                .as_ref()
                .map(CacheNamespace::as_str),
            want
        );
    }

    #[cfg(feature = "s3")]
    #[rstest::rstest]
    #[case::assumed_role(
        "123456789012",
        "arn:aws:sts::123456789012:assumed-role/reader/session",
        Some("aws.123456789012")
    )]
    #[case::user(
        "123456789012",
        "arn:aws-cn:iam::123456789012:user/alice",
        Some("aws-cn.123456789012")
    )]
    #[case::account_mismatch(
        "123456789012",
        "arn:aws:sts::210987654321:assumed-role/reader/session",
        None
    )]
    #[case::wrong_service("123456789012", "arn:aws:s3::123456789012:bucket", None)]
    #[case::regional("123456789012", "arn:aws:sts:us-east-1:123456789012:x", None)]
    #[case::not_an_arn("123456789012", "not-an-arn", None)]
    #[case::empty_resource("123456789012", "arn:aws:sts::123456789012:", None)]
    fn caller_identity_validates(
        #[case] account: &str,
        #[case] arn: &str,
        #[case] want: Option<&str>,
    ) {
        assert_eq!(
            CacheNamespace::from_caller_identity(account, arn)
                .as_ref()
                .map(CacheNamespace::as_str),
            want
        );
    }
}
