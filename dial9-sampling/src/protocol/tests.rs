use super::*;
use crate::backend::{ListPage, StoreFuture};
use crate::protocol::StartupFailure;
use crate::test_store::MemoryStore;
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

fn config(target: u32, host: &str) -> Config {
    crate::Sampling::builder()
        .scope("fleet/prod")
        .host_id(host)
        .target(target)
        .poll_interval(Duration::from_secs(1))
        .bucket_width(Duration::from_secs(10))
        .max_clock_skew(Duration::ZERO)
        .if_failed(StartupFailure::assume_population(u64::from(target.max(1))))
        .build()
        .policy()
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
    roster.cutoff_settled = true;
    roster.settled = true;
    roster.arm_cutoff = Cutoff::threshold(if population == 0 {
        config.tracked_limit()
    } else {
        (u128::from(config.target()) << 64) / u128::from(population)
    });
    roster.census_computed_at_ms = Some(now);
    roster.counts = Counts {
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
        fn list(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.0.list(request)
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
async fn oversized_active_cohort_is_trimmed_once_and_never_reenabled() {
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
    roster.cutoff_settled = true;
    roster.arm_cutoff = Cutoff(None);
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
        if !expected {
            recorder.enable();
            recorder.handle().enable();
            assert!(!recorder.handle().is_enabled());
        }
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
    roster.arm_cutoff = Cutoff(None);
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
    for session in sessions {
        session.shutdown(Duration::from_secs(1)).await;
    }
    #[cfg(feature = "dial9")]
    for (registration, _) in recorders {
        registration.stop();
    }
}

#[tokio::test(start_paused = true)]
async fn old_settled_cutoff_remains_usable_without_claiming_a_current_bound() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(1, "node");
    seed(&store, &policy, 1).await;
    tokio::time::advance(Duration::from_secs(86400 * 30)).await;
    let node = prepare(&policy, store.clone()).await;
    assert!(node.enabled);
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
async fn restart_invocation_ids_are_distinct_even_with_sticky_rank_and_delayed_cleanup() {
    let store = Arc::new(MemoryStore::new());
    let policy = config(15, "stable-host");
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
        millis(store.now()).unwrap() / 10000,
    )
    .await
    .unwrap();
    assert_eq!(
        before.len(),
        2,
        "overlapping processes are counted separately"
    );
    assert_eq!(before[0].rank, before[1].rank);
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
        census(store.as_ref(), &policy, current)
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
    fn list(&self, mut request: ListRequest) -> StoreFuture<'_, ListPage> {
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
            // Small pages force actual pagination. Reverse every page: sort
            // order is not part of the storage contract (S3 directory buckets).
            request = ListRequest::builder()
                .prefix(request.prefix())
                .page_size(2)
                .maybe_continuation(request.continuation().cloned())
                .build();
            let page = self.inner.list(request).await?;
            let mut objects = page.objects().to_vec();
            objects.reverse();
            Ok(ListPage::builder()
                .objects(objects)
                .maybe_continuation(page.continuation().cloned())
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
                .key(member_key(policy, bucket, rank, &state, invocation_id))
                .body(Bytes::new())
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn unordered_paginated_census_deduplicates_and_carrier_wins_same_bucket() {
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
    let result = census(&store, &policy, bucket).await.unwrap();
    assert_eq!(result.len(), 7);
    assert_eq!(
        result
            .iter()
            .find(|member| member.invocation_id == id)
            .unwrap()
            .state,
        MemberState::Carrier
    );
    assert!(store.lists.load(Ordering::SeqCst) >= 5);
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
    assert!(census(&store, &policy, bucket).await.is_err());
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
    compute(&mut roster, &policy, &members, &mut history, 1000);
    assert_eq!(roster.arm_cutoff, Cutoff::threshold(3 * (1u128 << 64) / 8));
    assert_eq!(
        roster.trim.as_ref().unwrap().survival,
        Cutoff::threshold(3 * (1u128 << 64) / 8)
    );
    let salt = roster.trim.as_ref().unwrap().salt.clone();
    compute(&mut roster, &policy, &members, &mut history, 2000);
    assert_eq!(
        roster.trim.as_ref().unwrap().salt,
        salt,
        "round held during follower grace"
    );
    compute(&mut roster, &policy, &members[..3], &mut history, 2500);
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

#[test]
fn sparse_census_uses_a_lower_population_assumption_and_default_tracks_all() {
    assert_eq!(config(15, "node").slice_bits(), 0);
    let mut policy = config(15, "node");
    policy.minimum_population = 100000;
    assert_eq!(policy.slice_bits(), 7);
    for index in 0..100 {
        policy.host_id = Some(format!("node-{index}"));
        if !policy.tracked() {
            assert!(!policy.failure_decision());
        }
    }
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
    let weights = crate::trace_weights::TraceWeights::new(policy.pool_hash(), 10.0);
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
        let joining_weights = crate::trace_weights::TraceWeights::new(policy.pool_hash(), 10.0);
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
    assert!(!decode(after, &policy).unwrap().roster.cutoff_settled);
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
    compute(&mut roster, &policy, &[], &mut history, 1000);
    assert!(!roster.cutoff_settled);
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
    assert!(!roster.cutoff_settled);
    assert!(roster.trim.is_none());
    assert_eq!(
        leader.pass_cadence(),
        Duration::from_secs(1),
        "renew sooner than the guard ends when lease is short"
    );
}

#[tokio::test(start_paused = true)]
async fn known_policy_mismatch_does_not_register_wrong_slice_membership() {
    let store = Arc::new(MemoryStore::new());
    let incumbent = config(15, "first");
    seed(&store, &incumbent, 100).await;
    let mut wrong = config(15, "other");
    wrong.minimum_population = 100000;
    // Pick a rank inside the new small slice so it reaches the startup GET.
    for index in 0..10000 {
        wrong.host_id = Some(format!("node-{index}"));
        if wrong.tracked() {
            break;
        }
    }
    assert!(wrong.tracked());
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
    let members = census(store.as_ref(), &policy, bucket).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].state, MemberState::Carrier);
}
