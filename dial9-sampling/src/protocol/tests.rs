use super::*;
use crate::backend::{ListPage, StoreFuture};
use crate::protocol::StartupFailure;
use crate::test_store::MemoryStore;
use sha2::Digest;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

impl Clock for MemoryStore {
    fn now(&self) -> SystemTime {
        self.now()
    }
}
async fn prepare(policy: &Config, store: Arc<MemoryStore>) -> PreparedSampling {
    prepare_using(
        policy.clone(),
        store.clone(),
        super::super::DEFAULT_PREPARE_TIMEOUT,
        store,
    )
    .await
}

fn config(target: u32, run: &str) -> Config {
    let mut policy = crate::Sampling::builder()
        .add_dimension("environment", "prod")
        .num_target_profilers(target)
        .poll_interval(Duration::from_secs(1))
        .bucket_width(Duration::from_secs(10))
        .max_clock_skew(Duration::ZERO)
        .if_failed(StartupFailure::assume_population(u64::from(target.max(1))))
        .build()
        .policy();
    let entropy: [u8; 16] = sha2::Sha256::digest(run.as_bytes())[..16]
        .try_into()
        .unwrap();
    policy.invocation_id = ulid::Ulid::from(u128::from_be_bytes(entropy)).to_string();
    policy
}
async fn settle_time(seconds: u64) {
    for _ in 0..seconds {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }
}
async fn publish(
    store: &MemoryStore,
    policy: &Config,
    roster: &Roster,
    condition: WriteCondition,
) -> VersionToken {
    match store
        .put(
            PutRequest::builder()
                .key(format!("{}roster.json", policy.pool_prefix()))
                .body(serialize(roster))
                .condition(condition)
                .build(),
        )
        .await
        .unwrap()
    {
        PutOutcome::Stored { version } => version,
        outcome => panic!("expected roster publication: {outcome:?}"),
    }
}

async fn seed(store: &MemoryStore, config: &Config, population: u64) -> Roster {
    let now = millis(store.now()).unwrap();
    let mut roster = Roster::initial(config, &nonce());
    roster.settled = true;
    roster.population = (population > 0).then(|| Population {
        reporting: roster.reporting.clone(),
        reporters: population,
        computed_ms: now,
    });
    roster.census_computed_at_ms = Some(now);
    roster.counts = Counts {
        observed: population,
        tracked: population,
        active: 0,
        carriers: 0,
        unarmed: population,
        no_support: 0,
    };
    publish(store, config, &roster, WriteCondition::absent()).await;
    roster
}

#[tokio::test(start_paused = true)]
async fn startup_requires_one_get_and_no_claims_or_lists() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "node");
    seed(&store, &policy, 10).await;
    let before = store.operation_count();
    let node = prepare(&policy, store.clone()).await;
    assert!(node.enabled);
    assert_eq!(&node.reason, &DecisionReason::Roster);
    assert_eq!(store.operation_count() - before, 1);
    assert!(!store.objects().keys().any(|k| k.contains("/members/")));
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[rstest::rstest]
#[case(100, 1000, 0)]
#[case(1000, 10000, 2000)]
#[tokio::test(start_paused = true)]
async fn backend_timestamp_uncertainty_rejects_an_impossible_census_window(
    #[case] poll_ms: u64,
    #[case] bucket_ms: u64,
    #[case] skew_ms: u64,
) {
    struct CoarseStore(MemoryStore);
    impl SamplingStore for CoarseStore {
        fn modification_time_uncertainty(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            self.0.get(key)
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            self.0.put(request)
        }
        fn list_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.0.list_page(request)
        }
        fn delete(&self, key: String) -> StoreFuture<'_, ()> {
            self.0.delete(key)
        }
    }
    let mut policy = config(15, "node");
    policy.poll_interval = Duration::from_millis(poll_ms);
    policy.bucket_width = Duration::from_millis(bucket_ms);
    policy.max_clock_skew = Duration::from_millis(skew_ms);
    assert!(
        policy.validate().is_ok(),
        "backend-independent settings are valid"
    );
    let store = Arc::new(CoarseStore(MemoryStore::new()));
    let node = prepare_using(
        policy,
        store.clone(),
        Duration::from_secs(1),
        Arc::new(WallClock),
    )
    .await;
    assert!(!node.enabled);
    assert!(matches!(
        node.reason,
        DecisionReason::InvalidConfiguration { .. }
    ));
    assert!(node.session.is_none());
    assert_eq!(store.0.operation_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn census_history_survives_roster_replacement_and_membership_cleanup() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(1, "historical-node");
    let node = prepare(&policy, store.clone()).await;
    settle_time(5).await;
    let time = store.now();
    let before = policy
        .population_at()
        .store(store.as_ref())
        .at(time)
        .max_age(Duration::from_secs(5))
        .call()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.tracked, 1);
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    assert!(!store.objects().keys().any(|k| k.contains("/members/")));
    store
        .delete(format!("{}roster.json", policy.pool_prefix()))
        .await
        .unwrap();
    let after = policy
        .population_at()
        .store(store.as_ref())
        .at(time)
        .max_age(Duration::from_secs(5))
        .call()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.population, before.population);
    assert_eq!(
        after.observation_completed_at,
        before.observation_completed_at
    );
}

#[rstest::rstest]
#[case(1)]
#[case(15)]
#[tokio::test(start_paused = true)]
async fn small_complete_pool_selects_everyone(#[case] population: u64) {
    let store = Arc::new(MemoryStore::new());
    let mut fleet = Vec::new();
    for index in 0..population {
        let policy = config(15, &format!("node-{index}"));
        if index == 0 {
            seed(&store, &policy, population).await;
        }
        let node = prepare(&policy, store.clone()).await;
        assert!(node.enabled);
        fleet.push(node);
    }
    for node in fleet {
        node.into_session()
            .unwrap()
            .shutdown(Duration::from_secs(1))
            .await;
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn zero_budget_has_no_get_but_constructed_store_still_participates(#[case] enabled: bool) {
    let store = Arc::new(MemoryStore::new());
    let mut policy = config(1, "node");
    if !enabled {
        policy.if_failed = StartupFailure::disabled();
    }
    let node = prepare_using(policy, store.clone(), Duration::ZERO, store.clone()).await;
    assert_eq!(node.enabled, enabled);
    assert_eq!(&node.reason, &DecisionReason::Timeout);
    assert_eq!(store.operation_count(), 0);
    assert!(node.session.is_some());
    settle_time(1).await;
    assert!(store.objects().keys().any(|key| key.contains("/members/")));
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[tokio::test(start_paused = true)]
async fn oversized_active_cohort_is_trimmed_once_without_sampler_reenable() {
    let store = Arc::new(MemoryStore::new());
    let mut sessions = Vec::new();
    let mut signals = Vec::new();
    let mut policies = Vec::new();
    #[cfg(feature = "dial9")]
    let mut recorders = Vec::new();
    for index in 0..12 {
        let mut policy = config(3, &format!("node-{index}"));
        policy.max_overshoot = Some(0);
        let node = prepare(&policy, store.clone()).await;
        assert!(node.enabled);
        let session = node.into_session().unwrap();
        #[cfg(feature = "dial9")]
        {
            let registration = Arc::new(crate::registration::RecorderRegistrations::new(Some(
                tokio::runtime::Handle::current(),
            )));
            let mut status = policy.status(true, &DecisionReason::MissingRoster, Some(&session));
            status.registration = Arc::downgrade(&registration);
            let recorder = dial9_core::recorder::recorder(
                dial9_core::buffer::MemoryBuffer::new(1 << 20).unwrap(),
            )
            .build();
            status.register_recorder(&recorder);
            recorders.push((registration, recorder));
        }
        signals.push(session.recording_changes());
        sessions.push(session);
        policies.push(policy);
    }
    let salt = nonce();
    let mut draws: Vec<_> = policies.iter().map(|p| p.draw("trim", &salt)).collect();
    draws.sort_unstable();
    let survival = Cutoff::threshold(u128::from(draws[6]));
    let expected: Vec<_> = policies
        .iter()
        .map(|p| survival.includes(p.draw("trim", &salt)))
        .collect();
    let mut roster = Roster::initial(&policies[0], &nonce());
    roster.epoch = 1;
    roster.census_computed_at_ms = Some(millis(store.now()).unwrap());
    roster.trim = Some(Trim {
        epoch: 1,
        salt,
        created_ms: roster.census_computed_at_ms.unwrap(),
        survival,
    });
    publish(&store, &policies[0], &roster, WriteCondition::absent()).await;
    settle_time(2).await;
    for (signal, expected) in signals.iter().zip(&expected) {
        assert_eq!(*signal.borrow(), *expected);
    }
    #[cfg(feature = "dial9")]
    for ((_, recorder), expected) in recorders.iter().zip(&expected) {
        assert_eq!(recorder.handle().is_enabled(), *expected);
    }
    assert_eq!(
        sessions
            .iter()
            .filter(|session| session.is_recording())
            .count(),
        expected.iter().filter(|&&kept| kept).count()
    );
    let terminal: Vec<_> = signals.iter().map(|signal| !*signal.borrow()).collect();
    // A later permissive roster cannot rearm a carrier.
    let mut policy = config(3, "reader");
    policy.max_overshoot = Some(0);
    let object = store
        .get(format!("{}roster.json", policy.pool_prefix()))
        .await
        .unwrap()
        .unwrap();
    let mut roster: Roster = serde_json::from_slice(object.body()).unwrap();
    roster.trim = None;
    roster.revision = nonce();
    store
        .put(
            PutRequest::builder()
                .key(format!("{}roster.json", policy.pool_prefix()))
                .body(serialize(&roster))
                .condition(WriteCondition::matching(object.version().clone()))
                .build(),
        )
        .await
        .unwrap();
    settle_time(2).await;
    for (was_terminal, signal) in terminal.iter().zip(&signals) {
        if *was_terminal {
            assert!(!*signal.borrow());
        }
    }
    #[cfg(feature = "dial9")]
    for ((_, recorder), expected) in recorders.iter().zip(&expected) {
        assert_eq!(recorder.handle().is_enabled(), *expected);
        if !expected {
            recorder.enable();
            assert!(recorder.handle().is_enabled());
        }
    }
    for session in sessions {
        session.shutdown(Duration::from_secs(1)).await;
    }
    #[cfg(feature = "dial9")]
    for (registration, recorder) in recorders {
        registration.stop();
        assert!(!recorder.handle().is_enabled());
    }
}

#[tokio::test(start_paused = true)]
async fn expired_population_observation_uses_the_startup_failure_policy() {
    let store = Arc::new(MemoryStore::new());
    let mut policy = config(1, "node");
    policy.if_failed = StartupFailure::disabled();
    seed(&store, &policy, 1).await;
    tokio::time::advance(Duration::from_secs(86400 * 30)).await;
    let node = prepare(&policy, store.clone()).await;
    assert!(!node.enabled);
    assert_eq!(node.reason, DecisionReason::UnsettledRoster);
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[tokio::test(start_paused = true)]
async fn configuration_mismatch_never_overwrites_the_existing_roster() {
    let store = Arc::new(MemoryStore::new());
    let first = config(1, "first");
    seed(&store, &first, 1).await;
    let key = format!("{}roster.json", first.pool_prefix());
    let before = store.get(key.clone()).await.unwrap().unwrap();
    let mut other = config(2, "other");
    other.if_failed = StartupFailure::disabled();
    let node = prepare(&other, store.clone()).await;
    assert!(!node.enabled);
    settle_time(12).await;
    assert_eq!(
        store.get(key).await.unwrap().unwrap().version(),
        before.version()
    );
    assert!(node.into_session().is_none());
}

#[tokio::test(start_paused = true)]
async fn restart_runs_have_distinct_ranks_and_membership_despite_delayed_cleanup() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "first-run");
    let old = prepare(&policy, store.clone()).await;
    settle_time(1).await;
    let old_keys: Vec<_> = store
        .objects()
        .into_keys()
        .filter(|key| key.contains("/members/"))
        .collect();
    let mut restarted = policy.clone();
    restarted.invocation_id = nonce();
    let new = prepare(&restarted, store.clone()).await;
    settle_time(1).await;
    let before = census(
        store.as_ref(),
        &policy,
        &ReportingEpoch::bootstrap(),
        millis(store.now()).unwrap() / 10000,
    )
    .await
    .unwrap();
    assert_eq!(
        before.len(),
        2,
        "overlapping processes are counted separately"
    );
    assert_ne!(before[0].rank, before[1].rank);
    old.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    for key in old_keys {
        store.delete(key).await.unwrap();
    }
    let after = census(
        store.as_ref(),
        &policy,
        &ReportingEpoch::bootstrap(),
        millis(store.now()).unwrap() / 10000,
    )
    .await
    .unwrap();
    assert_eq!(after.len(), 1);
    new.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[tokio::test(start_paused = true)]
async fn failed_cleanup_members_age_out_but_roster_is_never_deleted() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(1, "crashed");
    let node = prepare(&policy, store.clone()).await;
    settle_time(5).await;
    store.set_unavailable(true);
    node.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    store.set_unavailable(false);
    tokio::time::advance(Duration::from_secs(21)).await;
    let current = millis(store.now()).unwrap() / 10000;
    assert!(
        census(
            store.as_ref(),
            &policy,
            &ReportingEpoch::bootstrap(),
            current
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        store
            .objects()
            .keys()
            .any(|key| key.ends_with("roster.json"))
    );
}

#[tokio::test(start_paused = true)]
async fn actual_roster_mutations_change_body_etags_and_fence_paused_publishers() {
    let store = MemoryStore::new();
    let policy = config(15, "node");
    let mut roster = Roster::initial(&policy, &nonce());
    let owner = roster.holder.clone();
    let revision = roster.revision.clone();
    let first = publish(&store, &policy, &roster, WriteCondition::absent()).await;
    roster.mutate(&nonce()).unwrap();
    assert_ne!(revision, roster.revision);
    let second = publish(
        &store,
        &policy,
        &roster,
        WriteCondition::matching(first.clone()),
    )
    .await;
    roster.mutate(&owner).unwrap();
    let third = publish(&store, &policy, &roster, WriteCondition::matching(second)).await;
    assert_ne!(first, third);
    assert!(matches!(
        store
            .put(
                PutRequest::builder()
                    .key(format!("{}roster.json", policy.pool_prefix()))
                    .body(serialize(&roster))
                    .condition(WriteCondition::matching(first))
                    .build()
            )
            .await
            .unwrap(),
        PutOutcome::PreconditionFailed {}
    ));
}
#[tokio::test(start_paused = true)]
async fn takeover_waits_for_server_timestamp_precision_and_clock_skew_margins() {
    let store = Arc::new(MemoryStore::new());
    let mut policy = config(1, "node");
    policy.max_clock_skew = Duration::from_secs(2);
    let roster = seed(&store, &policy, 1).await;
    let observed = Observed {
        roster,
        version: VersionToken::new("v"),
        modified_at: store.now(),
    };
    assert!(!expired(
        &observed,
        &policy,
        store.as_ref(),
        store.now() + Duration::from_secs(5)
    ));
    assert!(expired(
        &observed,
        &policy,
        store.as_ref(),
        store.now() + Duration::from_millis(5001)
    ));
}

#[derive(Debug)]
struct Pages {
    inner: Arc<MemoryStore>,
    fail_second: AtomicBool,
    repeated_token: bool,
    lists: AtomicU64,
}
impl SamplingStore for Pages {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        self.inner.get(key)
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        self.inner.put(request)
    }
    fn delete(&self, key: String) -> StoreFuture<'_, ()> {
        self.inner.delete(key)
    }
    fn list_page(&self, mut request: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async move {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if request.continuation().is_some() && self.fail_second.load(Ordering::SeqCst) {
                return Err(StoreError::new(
                    StoreErrorKind::unavailable(),
                    "later page failed",
                ));
            }
            if self.repeated_token {
                return Ok(ListPage::builder()
                    .continuation(crate::backend::ContinuationToken::new("same"))
                    .build());
            }
            let offset = request.continuation().map_or(0, |token| {
                token
                    .as_str()
                    .strip_prefix("shuffled:")
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            });
            request = ListRequest::builder()
                .prefix(request.prefix())
                .page_size(usize::MAX)
                .build();
            let page = self.inner.list_page(request).await?;
            assert!(page.continuation().is_none());
            // Scramble the whole listing before pagination, then repeat one
            // key on a later page. Neither key order nor uniqueness is promised.
            let mut objects = page.objects().to_vec();
            objects.reverse();
            if !objects.is_empty() {
                objects.rotate_left(1);
                objects.push(objects[0].clone());
            }
            let length = objects.len();
            let page: Vec<_> = objects.into_iter().skip(offset).take(2).collect();
            let next = offset + page.len();
            let continuation = (next < length)
                .then(|| crate::backend::ContinuationToken::new(format!("shuffled:{next}")));
            Ok(ListPage::builder()
                .objects(page)
                .maybe_continuation(continuation)
                .build())
        })
    }
}
async fn put_member(
    store: &MemoryStore,
    policy: &Config,
    bucket: u64,
    rank: u64,
    state: MemberState,
    invocation_id: &str,
) {
    store
        .put(
            PutRequest::builder()
                .key(member_key(
                    policy,
                    "bootstrap",
                    bucket,
                    rank,
                    &state,
                    invocation_id,
                ))
                .body(Bytes::new())
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn shuffled_cross_page_census_deduplicates_and_carrier_wins_same_bucket() {
    let inner = Arc::new(MemoryStore::new());
    let policy = config(3, "reader");
    let bucket = millis(inner.now()).unwrap() / 10000;
    let id = nonce();
    put_member(&inner, &policy, bucket - 1, 5, MemberState::Active, &id).await;
    put_member(&inner, &policy, bucket, 5, MemberState::Active, &id).await;
    put_member(&inner, &policy, bucket, 5, MemberState::Carrier, &id).await;
    for rank in 6..12 {
        put_member(
            &inner,
            &policy,
            bucket,
            rank,
            MemberState::Unarmed,
            &nonce(),
        )
        .await;
    }
    let store = Pages {
        inner,
        fail_second: AtomicBool::new(false),
        repeated_token: false,
        lists: AtomicU64::new(0),
    };
    let result = census(&store, &policy, &ReportingEpoch::bootstrap(), bucket)
        .await
        .unwrap();
    assert_eq!(result.len(), 7);
    assert_eq!(
        result
            .iter()
            .find(|member| member.invocation_id == id)
            .unwrap()
            .state,
        MemberState::Carrier
    );
    assert_eq!(store.lists.load(Ordering::SeqCst), 6);
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn incomplete_or_looping_pagination_cannot_become_a_population(#[case] repeated: bool) {
    let inner = Arc::new(MemoryStore::new());
    let policy = config(3, "reader");
    let bucket = millis(inner.now()).unwrap() / 10000;
    for rank in 0..4 {
        put_member(
            &inner,
            &policy,
            bucket,
            rank,
            MemberState::Unarmed,
            &nonce(),
        )
        .await;
    }
    let store = Pages {
        inner,
        fail_second: AtomicBool::new(!repeated),
        repeated_token: repeated,
        lists: AtomicU64::new(0),
    };
    assert!(
        census(&store, &policy, &ReportingEpoch::bootstrap(), bucket)
            .await
            .is_err()
    );
    assert!(store.lists.load(Ordering::SeqCst) <= 4);
}

#[test]
fn count_cutoff_and_trim_survival_use_independent_probability_spaces() {
    let mut policy = config(3, "reader");
    policy.max_overshoot = Some(0);
    let mut roster = Roster::initial(&policy, &nonce());
    roster.epoch = 1;
    let members: Vec<_> = (0..8)
        .map(|rank| Member {
            rank,
            invocation_id: nonce(),
            bucket: 1,
            state: MemberState::Active,
        })
        .collect();
    let mut history = VecDeque::from([8, 8, 8]);
    compute(&mut roster, &policy, &members, &mut history, 1000, 5000);
    assert_eq!(
        roster
            .population
            .as_ref()
            .unwrap()
            .admission(policy.target()),
        Cutoff::threshold(3 * (1u128 << 64) / 8)
    );
    assert_eq!(
        roster.trim.as_ref().unwrap().survival,
        Cutoff::threshold(3 * (1u128 << 64) / 8)
    );
    let salt = roster.trim.as_ref().unwrap().salt.clone();
    compute(&mut roster, &policy, &members, &mut history, 2000, 5000);
    assert_eq!(
        roster.trim.as_ref().unwrap().salt,
        salt,
        "round held during follower grace"
    );
    compute(
        &mut roster,
        &policy,
        &members[..3],
        &mut history,
        2500,
        5000,
    );
    assert!(
        roster.trim.is_none(),
        "healthy census clears ancestral thinning"
    );
}

#[test]
fn policy_namespace_and_rank_use_unambiguous_sorted_dimensions() {
    let mut one = config(1, "node");
    one.dimensions = vec![("a".into(), "x\nb=y".into())];
    let mut two = one.clone();
    two.dimensions = vec![("a".into(), "x".into()), ("b".into(), "y".into())];
    assert_ne!(one.pool_prefix(), two.pool_prefix());
    assert_ne!(one.rank(), two.rank());
    let mut three = two.clone();
    three.dimensions.reverse();
    assert_eq!(two.pool_prefix(), three.pool_prefix());
    assert_eq!(two.rank(), three.rank());
}

#[rstest::rstest]
#[case(
    vec![("environment", "prod"), ("az", "us-east-1a")],
    "az=us-east-1a/environment=prod/"
)]
#[case(
    vec![("zone/name=%雪", "production/api=50%東京")],
    "zone%2Fname%3D%25雪=production%2Fapi%3D50%25東京/"
)]
#[case(vec![("a", "x\nb=y")], "a=x%0Ab%3Dy/")]
#[case(vec![], "")]
#[test]
fn pool_paths_are_sorted_readable_and_escape_names_and_values(
    #[case] dimensions: Vec<(&str, &str)>,
    #[case] expected: &str,
) {
    let mut policy = config(1, "node");
    policy.dimensions = dimensions
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    assert!(policy.validate().is_ok());
    assert_eq!(policy.pool_path(), expected);
    assert_eq!(policy.pool_prefix(), format!("v3/pools/{expected}"));
    assert_eq!(policy.members_prefix(), format!("v3/members/{expected}"));
    policy.dimensions.reverse();
    assert_eq!(policy.pool_path(), expected);
}

#[rstest::rstest]
#[case(vec![("a/b", "c")], vec![("a", "b/c")])]
#[case(vec![("a=b", "c")], vec![("a", "b=c")])]
#[case(vec![("a", "%2F")], vec![("a", "/")])]
#[case(vec![("a", "b/c=d")], vec![("a", "b"), ("c", "d")])]
#[case(vec![], vec![("ungrouped", "default")])]
#[test]
fn distinct_dimension_tuples_never_alias_path_delimiters(
    #[case] first: Vec<(&str, &str)>,
    #[case] second: Vec<(&str, &str)>,
) {
    let make = |dimensions: Vec<(&str, &str)>| {
        let mut policy = config(1, "node");
        policy.dimensions = dimensions
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        policy
    };
    assert_ne!(make(first).pool_path(), make(second).pool_path());
}

#[test]
fn fallback_admission_is_independent_of_reporting_eligibility() {
    let policy = config(15, "node");
    let reporting = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1)),
    };
    assert!(!reporting.cutoff.includes(policy.rank()));
    assert!(
        policy.failure_decision(),
        "fallback probability one includes quiet processes"
    );
}

fn worker(policy: &Config, store: Arc<dyn SamplingStore>, clock: Arc<dyn Clock>) -> Worker {
    let (signal, _receiver) = watch::channel(true);
    Worker::new(policy.clone(), store, clock, None, signal, true)
}

#[cfg(feature = "dial9")]
#[tokio::test(start_paused = true)]
async fn survivor_weights_apply_each_new_epoch_once_and_new_joiners_skip_old_rounds() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "weighted-worker");
    let mut active = worker(&policy, store.clone(), store.clone());
    let weights = crate::trace_weights::TraceWeights::new(policy.pool_path(), 10.0);
    active.weights = Some(weights.clone());
    let mut roster = Roster::initial(&policy, &nonce());
    roster.census_computed_at_ms = Some(millis(store.now()).unwrap());
    for epoch in [1, 2] {
        let salt = nonce();
        // Seal a nonzero cutoff above this fixture's known draw. Distribution
        // correctness is checked independently; this exercises real application.
        let cutoff = (u128::from(policy.draw("trim", &salt)) + (1u128 << 64)) / 2 + 1;
        let survival = Cutoff::threshold(cutoff);
        let factor = 1.0 / survival.probability();
        roster.epoch = epoch;
        roster.trim = Some(Trim {
            epoch,
            salt,
            created_ms: millis(store.now()).unwrap(),
            survival,
        });
        publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
        active.observed = active.read_roster().await.unwrap();
        let before = weights.current_expansion_weight().unwrap();
        active.apply_trim().await.unwrap();
        assert_eq!(weights.current_expansion_weight(), Some(before * factor));
        active.apply_trim().await.unwrap();
        assert_eq!(weights.current_expansion_weight(), Some(before * factor));
        let (signal, _) = watch::channel(true);
        let mut joining = Worker::new(
            config(3, "new-joiner"),
            store.clone(),
            store.clone(),
            active.observed.clone(),
            signal,
            true,
        );
        let joining_weights = crate::trace_weights::TraceWeights::new(policy.pool_path(), 10.0);
        joining.weights = Some(joining_weights.clone());
        joining.apply_trim().await.unwrap();
        assert_eq!(joining_weights.current_expansion_weight(), Some(10.0));
        assert_eq!(joining.state, MemberState::Active);
    }
}

#[tokio::test(start_paused = true)]
async fn committed_unknown_claim_is_not_retried_or_used_to_list() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "node");
    let mut candidate = worker(&policy, store.clone(), store.clone());
    store.lose_write_acknowledgements(1);
    let before = store.operation_count();
    assert!(candidate.election().await.is_err());
    assert!(candidate.leader.is_none());
    assert_eq!(
        store.operation_count() - before,
        2,
        "one reread and one conditional PUT, no blind retry"
    );
    let key = format!("{}roster.json", policy.pool_prefix());
    let committed = store.get(key.clone()).await.unwrap().unwrap();
    assert_eq!(
        decode(committed.clone(), &policy).unwrap().roster.holder,
        candidate.invocation_id
    );
    candidate.observed = candidate.read_roster().await.unwrap();
    candidate.election().await.unwrap();
    assert!(candidate.leader.is_none());
    assert_eq!(
        store.get(key).await.unwrap().unwrap().version(),
        committed.version()
    );
    candidate.poll().await.unwrap();
    assert!(
        candidate.leader.is_some(),
        "a GET reconciles committed own claim"
    );
}

#[tokio::test(start_paused = true)]
async fn actual_leader_pass_from_a_paused_owner_cannot_publish_after_takeover() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "node");
    let mut old = worker(&policy, store.clone(), store.clone());
    old.election().await.unwrap();
    assert!(old.leader.is_some());
    tokio::time::advance(Duration::from_secs(4)).await;
    let mut new = worker(&policy, store.clone(), store.clone());
    new.election().await.unwrap();
    assert!(new.leader.is_some());
    let key = format!("{}roster.json", policy.pool_prefix());
    let takeover = store.get(key.clone()).await.unwrap().unwrap();
    old.leader_pass().await.unwrap();
    assert!(old.leader.is_none());
    assert_eq!(
        store.get(key).await.unwrap().unwrap().version(),
        takeover.version()
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_second_page_cannot_publish_a_leader_count_or_cutoff() {
    let inner = Arc::new(MemoryStore::new());
    let policy = config(3, "reader");
    let pages = Arc::new(Pages {
        inner: inner.clone(),
        fail_second: AtomicBool::new(true),
        repeated_token: false,
        lists: AtomicU64::new(0),
    });
    let mut leader = worker(&policy, pages, inner.clone());
    leader.election().await.unwrap();
    let bucket = millis(inner.now()).unwrap() / 10000;
    for rank in 0..4 {
        put_member(
            &inner,
            &policy,
            bucket,
            rank,
            MemberState::Unarmed,
            &nonce(),
        )
        .await;
    }
    let key = format!("{}roster.json", policy.pool_prefix());
    let before = inner.get(key.clone()).await.unwrap().unwrap();
    assert!(leader.leader_pass().await.is_err());
    assert!(leader.leader.is_none());
    let after = inner.get(key).await.unwrap().unwrap();
    assert_eq!(before.version(), after.version());
    assert!(decode(after, &policy).unwrap().roster.population.is_none());
}

#[tokio::test(start_paused = true)]
async fn partition_can_exceed_soft_cap_then_recovery_only_stops_processes() {
    let store = Arc::new(MemoryStore::new());
    let mut fleet = Vec::new();
    store.set_unavailable(true);
    for index in 0..12 {
        let mut policy = config(3, &format!("node-{index}"));
        policy.max_overshoot = Some(0);
        let node = prepare(&policy, store.clone()).await;
        assert!(node.enabled);
        fleet.push(node.into_session().unwrap());
    }
    settle_time(8).await;
    assert_eq!(
        fleet
            .iter()
            .filter(|session| session.is_recording())
            .count(),
        12
    );
    store.set_unavailable(false);
    settle_time(16).await;
    // Statistical concentration is checked by the independent simulator. This
    // regression verifies recovery executes the census and publishes thinning.
    let object = store
        .get(format!("{}roster.json", config(3, "reader").pool_prefix()))
        .await
        .unwrap()
        .unwrap();
    let roster: Roster = serde_json::from_slice(object.body()).unwrap();
    assert!(roster.census_computed_at_ms.is_some());
    for session in fleet {
        session.shutdown(Duration::from_secs(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn stale_keep_boundary_does_not_trim_using_an_old_cohort() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "node");
    let mut roster = seed(&store, &policy, 3).await;
    roster.epoch = 1;
    roster.trim = Some(Trim {
        epoch: 1,
        salt: nonce(),
        created_ms: millis(store.now()).unwrap(),
        survival: Cutoff(Some(1)),
    });
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    let mut active = worker(&policy, store.clone(), store.clone());
    active.observed = active.read_roster().await.unwrap();
    active.apply_trim().await.unwrap();
    assert_eq!(active.state, MemberState::Active);
}

#[tokio::test(start_paused = true)]
async fn prepared_startup_choice_is_immutable_even_if_maintenance_trims_before_callback() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "node");
    let mut roster = seed(&store, &policy, 3).await;
    let prepared = prepare(&policy, store.clone()).await;
    assert!(prepared.enabled);
    roster.epoch = 1;
    roster.trim = Some(Trim {
        epoch: 1,
        salt: nonce(),
        created_ms: millis(store.now()).unwrap(),
        survival: Cutoff(Some(1)),
    });
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    settle_time(1).await;
    assert!(!prepared.session.as_ref().unwrap().is_recording());
    assert!(
        prepared.enabled,
        "callback sees the original startup choice"
    );
    prepared
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[test]
fn empty_or_unstable_census_never_creates_a_usable_full_range_cutoff() {
    let policy = config(15, "node");
    let mut roster = Roster::initial(&policy, &nonce());
    let mut history = VecDeque::from([0, 0, 0]);
    compute(&mut roster, &policy, &[], &mut history, 1000, 5000);
    assert!(roster.population.is_none());
    assert!(!roster.settled);
    assert!(roster.census_computed_at_ms.is_some());
    assert_eq!(
        roster.counts.tracked, 0,
        "zero is only a successfully counted empty census"
    );
}

struct FixedClock(SystemTime);
impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

#[tokio::test(start_paused = true)]
async fn bucket_edge_renews_lease_without_listing_or_replacing_admission() {
    let store = Arc::new(MemoryStore::new());
    let mut policy = config(15, "node");
    policy.max_clock_skew = Duration::from_secs(2);
    let clock = Arc::new(FixedClock(UNIX_EPOCH + Duration::from_millis(29999)));
    let mut leader = worker(&policy, store.clone(), clock);
    leader.election().await.unwrap();
    assert!(leader.leader.is_some());
    let before = store.operation_count();
    leader.leader_pass().await.unwrap();
    assert_eq!(
        store.operation_count() - before,
        1,
        "lease-only CAS; no guarded LIST pages"
    );
    let roster = &leader.leader.as_ref().unwrap().roster;
    assert!(roster.population.is_none());
    assert!(roster.trim.is_none());
    assert_eq!(
        leader.pass_cadence(),
        Duration::from_secs(1),
        "renew sooner than the guard ends when lease is short"
    );
}

#[tokio::test(start_paused = true)]
async fn known_policy_mismatch_does_not_register_membership() {
    let store = Arc::new(MemoryStore::new());
    let incumbent = config(15, "first");
    seed(&store, &incumbent, 100).await;
    let mut wrong = config(15, "other");
    wrong.max_overshoot = Some(99);
    let node = prepare(&wrong, store.clone()).await;
    assert_eq!(&node.reason, &DecisionReason::IncompatiblePolicy);
    assert!(node.into_session().is_none());
    assert!(!store.objects().keys().any(|key| key.contains("/members/")));
}

#[tokio::test(start_paused = true)]
async fn fresh_server_commit_cannot_freshen_an_expired_census_trim_boundary() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "node");
    let mut roster = seed(&store, &policy, 3).await;
    roster.epoch = 1;
    roster.trim = Some(Trim {
        epoch: 1,
        salt: nonce(),
        created_ms: millis(store.now()).unwrap(),
        survival: Cutoff(Some(1)),
    });
    let key = format!("{}roster.json", policy.pool_prefix());
    let previous = store.get(key.clone()).await.unwrap().unwrap();
    let request = PutRequest::builder()
        .key(&key)
        .body(serialize(&roster))
        .condition(WriteCondition::matching(previous.version().clone()))
        .build();
    // No takeover occurs while the publisher is paused. Its CAS can still commit,
    // but a new server LastModified must not rejuvenate this old census.
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(matches!(
        store.put(request).await.unwrap(),
        PutOutcome::Stored { .. }
    ));
    let mut active = worker(&policy, store.clone(), store.clone());
    active.observed = active.read_roster().await.unwrap();
    assert!(!expired(
        active.observed.as_ref().unwrap(),
        &policy,
        store.as_ref(),
        store.now()
    ));
    active.apply_trim().await.unwrap();
    assert_eq!(active.state, MemberState::Active);
}

#[tokio::test(start_paused = true)]
async fn a_newer_bucket_active_write_cannot_override_a_terminal_carrier_invocation_id() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(3, "node");
    let bucket = millis(store.now()).unwrap() / 10000;
    let id = nonce();
    // A bounded backwards clock adjustment can publish carrier in the prior
    // bucket after active was already written in the newer one.
    put_member(&store, &policy, bucket, 5, MemberState::Active, &id).await;
    put_member(&store, &policy, bucket - 1, 5, MemberState::Carrier, &id).await;
    let members = census(
        store.as_ref(),
        &policy,
        &ReportingEpoch::bootstrap(),
        bucket,
    )
    .await
    .unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].state, MemberState::Carrier);
}

#[test]
fn census_excludes_extra_profilers_but_thinning_counts_them() {
    let mut policy = config(3, "counter");
    policy.max_overshoot = Some(0);
    let mut roster = Roster::initial(&policy, &nonce());
    roster.reporting = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1 << 63)),
    };
    roster.epoch = 1;
    let members: Vec<_> = (0..10)
        .map(|i| Member {
            rank: if i < 2 { i } else { (1 << 63) + i },
            invocation_id: nonce(),
            bucket: 1,
            state: MemberState::Active,
        })
        .collect();
    let mut history = VecDeque::from([2, 2, 2]);
    compute(&mut roster, &policy, &members, &mut history, 1000, 5000);
    assert_eq!(roster.counts.tracked, 2);
    assert_eq!(roster.counts.observed, 10);
    assert_eq!(roster.counts.active, 10);
    assert_eq!(roster.population.as_ref().unwrap().estimate(), 4.0);
    assert_eq!(
        roster
            .population
            .as_ref()
            .unwrap()
            .admission(3)
            .probability(),
        0.75,
    );
    assert_eq!(
        roster.trim.as_ref().unwrap().survival,
        Cutoff::threshold(3 * (1u128 << 64) / 10),
    );
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn pending_epochs_change_reporting_without_rerolling_profiling(#[case] profiling: bool) {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "new-reporter");
    let mut roster = Roster::initial(&policy, &nonce());
    // This run is outside the active census and inside the expanded pending census.
    roster.reporting = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1)),
    };
    let (signal, _) = watch::channel(profiling);
    let mut process = Worker::new(
        policy.clone(),
        store.clone(),
        store.clone(),
        None,
        signal,
        true,
    );
    #[cfg(feature = "dial9")]
    if profiling {
        process.weights = Some(crate::trace_weights::TraceWeights::new(
            policy.pool_path(),
            10.0,
        ));
    }
    publish(&store, &policy, &roster, WriteCondition::absent()).await;
    process.observed = process.read_roster().await.unwrap();
    let before = store.operation_count();
    process.refresh_member(false).await.unwrap();
    assert_eq!(store.operation_count() - before, u64::from(profiling));

    let next = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(None),
    };
    let now = millis(store.now()).unwrap();
    roster.pending = Some(Handoff {
        reporting: next.clone(),
        created_ms: now,
        collect_after_ms: now + 5000,
    });
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    process.poll().await.unwrap();
    assert_eq!(*process.signal.borrow(), profiling);
    #[cfg(feature = "dial9")]
    if profiling {
        assert_eq!(
            process.weights.as_ref().unwrap().current_expansion_weight(),
            Some(10.0)
        );
    }
    let bucket = process.bucket().unwrap();
    let current = census(store.as_ref(), &policy, &roster.reporting, bucket)
        .await
        .unwrap();
    let pending = census(store.as_ref(), &policy, &next, bucket)
        .await
        .unwrap();
    assert_eq!(current.len(), usize::from(profiling));
    assert_eq!(
        pending.len(),
        1,
        "new reporters write immediately in the pending namespace"
    );
    assert_eq!(
        process.state,
        if profiling {
            MemberState::Active
        } else {
            MemberState::Unarmed
        }
    );
}

#[tokio::test(start_paused = true)]
async fn handoff_waits_for_collection_and_never_divides_old_members_by_the_new_fraction() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "leader");
    let now = millis(store.now()).unwrap();
    let mut roster = seed(&store, &policy, 100).await;
    let next = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1 << 63)),
    };
    roster.pending = Some(Handoff {
        reporting: next.clone(),
        created_ms: now,
        collect_after_ms: now + 5000,
    });
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    let mut leader = worker(&policy, store.clone(), store.clone());
    leader.observed = leader.read_roster().await.unwrap();
    leader.leader = leader.observed.clone();
    let bucket = leader.bucket().unwrap();
    for i in 0..100 {
        let rank = (i * (1u128 << 64) / 100) as u64;
        put_member(
            &store,
            &policy,
            bucket,
            rank,
            MemberState::Unarmed,
            &nonce(),
        )
        .await;
    }
    for _ in 0..4 {
        leader.leader_pass().await.unwrap();
    }
    assert_eq!(
        leader.leader.as_ref().unwrap().roster.reporting.id,
        "bootstrap"
    );
    assert_eq!(
        leader
            .leader
            .as_ref()
            .unwrap()
            .roster
            .population
            .as_ref()
            .unwrap()
            .estimate(),
        100.0
    );

    tokio::time::advance(Duration::from_secs(6)).await;
    let bucket = leader.bucket().unwrap();
    for i in 0..50 {
        let key = member_key(
            &policy,
            &next.id,
            bucket,
            (i * (1u128 << 64) / 100) as u64,
            &MemberState::Unarmed,
            &nonce(),
        );
        store
            .put(
                PutRequest::builder()
                    .key(key)
                    .body(Bytes::new())
                    .condition(WriteCondition::unconditional())
                    .build(),
            )
            .await
            .unwrap();
    }
    for _ in 0..3 {
        leader.leader_pass().await.unwrap();
        assert!(leader.leader.as_ref().unwrap().roster.pending.is_some());
    }
    leader.leader_pass().await.unwrap();
    let promoted = &leader.leader.as_ref().unwrap().roster;
    assert_eq!(promoted.reporting.id, next.id);
    assert!(promoted.pending.is_none());
    assert_eq!(promoted.counts.tracked, 50);
    assert_eq!(promoted.population.as_ref().unwrap().estimate(), 100.0);
    assert!(promoted.trim.is_none());
}

#[tokio::test(start_paused = true)]
async fn quiet_process_recovers_a_pool_after_every_previous_reporter_is_lost() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "quiet-survivor");
    let mut roster = seed(&store, &policy, 100_000).await;
    roster.reporting = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1)),
    };
    let old_population = roster.population.as_ref().unwrap().estimate();
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    let (signal, _) = watch::channel(false);
    let mut process = Worker::new(
        policy.clone(),
        store.clone(),
        store.clone(),
        None,
        signal,
        true,
    );
    process.observed = process.read_roster().await.unwrap();
    let before = store.operation_count();
    process.refresh_member(false).await.unwrap();
    assert_eq!(
        store.operation_count(),
        before,
        "quiet survivor has no active-epoch heartbeat"
    );
    process.election().await.unwrap();
    assert!(
        process.leader.is_some(),
        "census eligibility must not gate election"
    );
    process.leader_pass().await.unwrap();
    let recovering = &process.leader.as_ref().unwrap().roster;
    assert_eq!(recovering.counts.tracked, 0);
    assert_eq!(
        recovering.population.as_ref().unwrap().estimate(),
        old_population
    );
    assert_eq!(
        recovering.pending.as_ref().unwrap().reporting.cutoff,
        Cutoff(None)
    );
    assert_eq!(
        recovering
            .admission(&policy, millis(store.now()).unwrap())
            .unwrap(),
        Cutoff::threshold((u128::from(policy.target()) << 64) / 100_000),
    );
    process.refresh_member(false).await.unwrap();
    let next = process
        .observed
        .as_ref()
        .unwrap()
        .roster
        .pending
        .as_ref()
        .unwrap()
        .reporting
        .clone();
    assert_eq!(
        census(store.as_ref(), &policy, &next, process.bucket().unwrap())
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        !*process.signal.borrow(),
        "recovery never turns a previously off process on"
    );
}

#[tokio::test(start_paused = true)]
async fn a_thinned_profiler_outside_the_census_stops_writing_after_active_ghosts_expire() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(1, "terminal-profiler");
    let mut roster = Roster::initial(&policy, &nonce());
    roster.reporting = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1)),
    };
    roster.epoch = 1;
    let now = millis(store.now()).unwrap();
    roster.census_computed_at_ms = Some(now);
    roster.trim = Some(Trim {
        epoch: 1,
        salt: nonce(),
        created_ms: now,
        survival: Cutoff(Some(1)),
    });
    publish(&store, &policy, &roster, WriteCondition::absent()).await;
    let mut process = worker(&policy, store.clone(), store.clone());
    process.observed = process.read_roster().await.unwrap();
    process.refresh_member(false).await.unwrap();
    process.apply_trim().await.unwrap();
    assert_eq!(process.state, MemberState::Carrier);
    tokio::time::advance(Duration::from_secs(21)).await;
    let before = store.operation_count();
    process.refresh_member(false).await.unwrap();
    assert_eq!(store.operation_count(), before);
    assert!(
        census(
            store.as_ref(),
            &policy,
            &roster.reporting,
            process.bucket().unwrap()
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn quiet_takeover_preserves_an_in_progress_reporting_handoff() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "replacement");
    let mut roster = seed(&store, &policy, 100_000).await;
    roster.reporting = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1)),
    };
    let now = millis(store.now()).unwrap();
    let next = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(None),
    };
    roster.pending = Some(Handoff {
        reporting: next.clone(),
        created_ms: now,
        collect_after_ms: now + 5000,
    });
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    let (signal, _) = watch::channel(false);
    let mut replacement = Worker::new(
        policy.clone(),
        store.clone(),
        store.clone(),
        None,
        signal,
        true,
    );
    replacement.observed = replacement.read_roster().await.unwrap();
    replacement.election().await.unwrap();
    let claimed = &replacement.leader.as_ref().unwrap().roster;
    assert_eq!(claimed.reporting, roster.reporting);
    assert_eq!(claimed.pending.as_ref().unwrap().reporting, next);
    assert_eq!(
        claimed.pending.as_ref().unwrap().collect_after_ms,
        now + 5000
    );
    assert_eq!(claimed.population.as_ref().unwrap().estimate(), 100_000.0);
}

#[tokio::test(start_paused = true)]
async fn failed_pending_census_cannot_publish_or_promote_partial_membership() {
    struct RejectPending {
        inner: Arc<MemoryStore>,
        id: String,
    }
    impl SamplingStore for RejectPending {
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            self.inner.get(key)
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            self.inner.put(request)
        }
        fn list_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            Box::pin(async move {
                if request.prefix().contains(&format!("/{}/", self.id)) {
                    return Err(StoreError::new(
                        StoreErrorKind::unavailable(),
                        "pending LIST unavailable",
                    ));
                }
                self.inner.list_page(request).await
            })
        }
        fn delete(&self, key: String) -> StoreFuture<'_, ()> {
            self.inner.delete(key)
        }
    }
    let inner = Arc::new(MemoryStore::new());
    let policy = config(15, "leader");
    let mut roster = seed(&inner, &policy, 100).await;
    let now = millis(inner.now()).unwrap();
    let next = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1 << 63)),
    };
    roster.pending = Some(Handoff {
        reporting: next.clone(),
        created_ms: now - 10000,
        collect_after_ms: now - 5000,
    });
    publish(&inner, &policy, &roster, WriteCondition::unconditional()).await;
    let store = Arc::new(RejectPending {
        inner: inner.clone(),
        id: next.id,
    });
    let mut leader = worker(&policy, store, inner.clone());
    leader.observed = leader.read_roster().await.unwrap();
    leader.leader = leader.observed.clone();
    let version = leader.observed.as_ref().unwrap().version.clone();
    assert!(leader.leader_pass().await.is_err());
    let current = leader.read_roster().await.unwrap().unwrap();
    assert_eq!(current.version, version);
    assert_eq!(current.roster.reporting, roster.reporting);
    assert!(current.roster.pending.is_some());
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn zero_reporters_replace_a_stalled_handoff_and_fence_its_old_census(
    #[case] pending_reporter: bool,
) {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "recovering-leader");
    let mut roster = seed(&store, &policy, 100_000).await;
    let current = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1 << 63)),
    };
    let abandoned = ReportingEpoch {
        id: nonce(),
        cutoff: Cutoff(Some(1 << 62)),
    };
    roster.reporting = current.clone();
    let now = millis(store.now()).unwrap();
    roster.pending = Some(Handoff {
        reporting: abandoned.clone(),
        created_ms: now - 10_000,
        collect_after_ms: now - 5000,
    });
    publish(&store, &policy, &roster, WriteCondition::unconditional()).await;
    let mut leader = worker(&policy, store.clone(), store.clone());
    leader.observed = leader.read_roster().await.unwrap();
    leader.leader = leader.observed.clone();
    if pending_reporter {
        let key = member_key(
            &policy,
            &abandoned.id,
            leader.bucket().unwrap(),
            0,
            &MemberState::Unarmed,
            &nonce(),
        );
        store
            .put(
                PutRequest::builder()
                    .key(key)
                    .body(Bytes::new())
                    .condition(WriteCondition::unconditional())
                    .build(),
            )
            .await
            .unwrap();
        leader.pending_history_epoch = Some(abandoned.id.clone());
        leader.pending_history = VecDeque::from([1, 1, 1]);
    }
    leader.leader_pass().await.unwrap();
    let recovering = &leader.leader.as_ref().unwrap().roster;
    assert_eq!(recovering.reporting, current);
    let recovery = &recovering.pending.as_ref().unwrap().reporting;
    assert_ne!(recovery.id, abandoned.id);
    assert_eq!(recovery.cutoff, Cutoff(None));
    assert_eq!(
        recovering.population.as_ref().unwrap().estimate(),
        100_000.0
    );
}
