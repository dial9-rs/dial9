use super::*;
use crate::{
    backend::{ContinuationToken, ListPage, ListRequest, ListedObject, StoreFuture},
    protocol::{Clock, prepare_using},
    test_store::{MemoryStore, StoreOperation},
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

impl Clock for MemoryStore {
    fn now(&self) -> SystemTime {
        self.now()
    }
}

/// Storage events let paused tests wait for actual requests without scheduling guesses.
struct NotifyingStore {
    inner: Arc<MemoryStore>,
    puts: tokio::sync::mpsc::UnboundedSender<(String, bool)>,
    active: AtomicUsize,
}
struct ActiveOperation<'a>(&'a AtomicUsize);
impl Drop for ActiveOperation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl NotifyingStore {
    fn operation(&self) -> ActiveOperation<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        ActiveOperation(&self.active)
    }
}
impl SamplingStore for NotifyingStore {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            let _active = self.operation();
            self.inner.get(key).await
        })
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        Box::pin(async move {
            let _active = self.operation();
            let key = request.key().to_owned();
            self.puts.send((key.clone(), false)).unwrap();
            let outcome = self.inner.put(request).await?;
            self.puts.send((key, true)).unwrap();
            Ok(outcome)
        })
    }
    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        Box::pin(async move {
            let _active = self.operation();
            self.inner.list_sorted_page(request).await
        })
    }
}
async fn sleep_until(store: &MemoryStore, ms: u64) {
    let target = UNIX_EPOCH + Duration::from_millis(ms);
    tokio::time::sleep(target.duration_since(store.now()).unwrap()).await;
}
fn metadata_weight(weights: &crate::trace_weights::TraceWeights) -> String {
    use dial9_core::source::Source;
    let mut entries = Vec::new();
    weights.clone().segment_metadata(&mut entries);
    entries
        .into_iter()
        .find(|(key, _)| key == "dial9.sampling.weight")
        .unwrap()
        .1
}
fn config_for(target: u32, name: &str) -> Config {
    let mut config = crate::Sampling::builder()
        .add_dimension("environment", "prod")
        .num_target_profilers(target)
        .if_failed(super::super::StartupFailure::assume_population(u64::from(
            target.max(1),
        )))
        .build()
        .policy();
    config.invocation_id = ulid::Ulid::from(u128::from_be_bytes(
        Sha256::digest(name.as_bytes())[..16].try_into().unwrap(),
    ))
    .to_string();
    config
}
fn store_at(ms: u64) -> Arc<MemoryStore> {
    Arc::new(MemoryStore::at(UNIX_EPOCH + Duration::from_millis(ms)))
}
fn worker(config: &Config, store: Arc<MemoryStore>, enabled: bool) -> Worker {
    Worker::new(
        config.clone(),
        store.clone(),
        store,
        None,
        watch::channel(enabled).0,
        false,
    )
    .unwrap()
}
async fn publish(
    store: &dyn SamplingStore,
    config: &Config,
    roster: &Roster,
    condition: WriteCondition,
) -> VersionToken {
    match store
        .put(
            PutRequest::builder()
                .key(format!("{}roster.json", config.pool_prefix()))
                .body(serialize(roster))
                .condition(condition)
                .build(),
        )
        .await
        .unwrap()
    {
        PutOutcome::Stored { version } => version,
        other => panic!("expected successful publication: {other:?}"),
    }
}
async fn report(
    store: &dyn SamplingStore,
    config: &Config,
    channel: &str,
    window: u64,
    rank: u64,
    cutoff: Cutoff,
) {
    store
        .put(
            PutRequest::builder()
                .key(census::report_key(config, channel, window, rank, &cutoff))
                .body(Bytes::new())
                .build(),
        )
        .await
        .unwrap();
}
fn page(keys: Vec<String>, continuation: Option<&str>) -> ListPage {
    ListPage::builder()
        .objects(
            keys.into_iter()
                .map(|key| ListedObject::builder().key(key).build())
                .collect(),
        )
        .maybe_continuation(continuation.map(ContinuationToken::new))
        .build()
}
struct ScriptStore {
    inner: MemoryStore,
    pages: Mutex<VecDeque<ListPage>>,
    lists: Mutex<Vec<ListRequest>>,
    get_failure: AtomicBool,
    puts: AtomicUsize,
}
impl ScriptStore {
    fn new(pages: Vec<ListPage>) -> Self {
        Self {
            inner: MemoryStore::at(UNIX_EPOCH + Duration::from_secs(600)),
            pages: Mutex::new(pages.into()),
            lists: Mutex::new(Vec::new()),
            get_failure: AtomicBool::new(false),
            puts: AtomicUsize::new(0),
        }
    }
}
impl Clock for ScriptStore {
    fn now(&self) -> SystemTime {
        self.inner.now()
    }
}
impl SamplingStore for ScriptStore {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            if self.get_failure.load(Ordering::SeqCst) {
                return Err(StoreError::new(
                    StoreErrorKind::unavailable(),
                    "scripted GET failure",
                ));
            }
            self.inner.get(key).await
        })
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(request)
    }
    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        self.lists.lock().unwrap().push(request);
        Box::pin(async move {
            self.pages
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| invalid("unexpected replacement LIST"))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn startup_is_one_read_and_expired_source_uses_configured_fallback() {
    let store = store_at(600_000);
    let mut config = config_for(15, "startup");
    config.if_failed = super::super::StartupFailure::disabled();
    let mut roster = Roster::initial(&config, &nonce());
    roster.population = Some(Population {
        window: 8,
        estimate: 10.0,
        computed_ms: 560_000,
    });
    publish(store.as_ref(), &config, &roster, WriteCondition::absent()).await;
    let before = store.operation_count();
    let prepared = prepare_using(
        config.clone(),
        store.clone(),
        Duration::from_secs(1),
        store.clone(),
    )
    .await;
    assert!(prepared.enabled);
    assert_eq!(prepared.reason, DecisionReason::Roster);
    assert_eq!(store.operation_count() - before, 1);
    prepared
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    tokio::time::advance(Duration::from_secs(600)).await;
    let prepared =
        prepare_using(config, store.clone(), Duration::from_secs(1), store.clone()).await;
    assert!(!prepared.enabled);
    assert_eq!(prepared.reason, DecisionReason::UnsettledRoster);
    prepared
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
}

#[tokio::test(start_paused = true)]
async fn bottom_sample_uses_each_actual_cutoff_without_a_second_division() {
    let store = store_at(600_000);
    let config = config_for(15, "one");
    report(
        store.as_ref(),
        &config,
        "census",
        8,
        u64::MAX / 8,
        Cutoff::threshold(SPACE / 2),
    )
    .await;
    report(
        store.as_ref(),
        &config_for(15, "two"),
        "census",
        8,
        u64::MAX / 16,
        Cutoff::threshold(SPACE / 4),
    )
    .await;
    let sample = census::observe(
        store.as_ref(),
        &config,
        "census",
        8,
        480,
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(sample.retained, 2);
    assert_eq!(sample.estimate, 6.0);
}

#[tokio::test(start_paused = true)]
async fn strict_boundary_excludes_all_equal_rank_ties() {
    let store = store_at(600_000);
    let policy = config_for(15, "root");
    for (i, rank) in [SPACE / 4, SPACE / 2, SPACE / 2].into_iter().enumerate() {
        report(
            store.as_ref(),
            &config_for(15, &i.to_string()),
            "census",
            8,
            rank as u64,
            Cutoff(None),
        )
        .await;
    }
    let sample = census::observe(
        store.as_ref(),
        &policy,
        "census",
        8,
        2,
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(sample.retained, 1);
    assert_eq!(sample.estimate, 2.0);
}

#[tokio::test(start_paused = true)]
async fn target_fifteen_never_lists_more_than_546_keys_in_two_calls() {
    let store = store_at(622_000);
    let policy = config_for(15, "leader");
    for i in 0..2000 {
        let member = config_for(15, &format!("member-{i}"));
        report(
            store.as_ref(),
            &member,
            "census",
            9,
            i * (u64::MAX / 2001),
            Cutoff(None),
        )
        .await;
        report(
            store.as_ref(),
            &member,
            "profilers",
            9,
            i * (u64::MAX / 2001),
            Cutoff(None),
        )
        .await;
    }
    let mut node = worker(&policy, store.clone(), false);
    node.election_needed = true;
    node.election().await.unwrap();
    let before = store.operation_count();
    node.leader_pass().await.unwrap();
    // GET, reservation CAS, two bounded LISTs, result CAS.
    assert_eq!(store.operation_count() - before, 5);
    let roster = node.leader.as_ref().unwrap().roster.clone();
    assert_eq!(roster.last_window, Some(9));
    assert!(roster.population.as_ref().unwrap().estimate > 1900.0);
    let before = store.operation_count();
    node.leader_pass().await.unwrap();
    assert_eq!(
        store.operation_count() - before,
        2,
        "renewing a reserved window does not re-LIST"
    );
    assert_eq!(
        census::observe(
            store.as_ref(),
            &policy,
            "census",
            9,
            480,
            Duration::from_secs(3)
        )
        .await
        .unwrap()
        .retained,
        480
    );
}

#[tokio::test(start_paused = true)]
async fn larger_target_has_a_finite_page_budget_and_remaining_record_limit() {
    let config = config_for(40, "leader");
    let keys: Vec<_> = (0..1281)
        .map(|i| {
            census::report_key(
                &config_for(40, &i.to_string()),
                "census",
                8,
                i,
                &Cutoff(None),
            )
        })
        .collect();
    let store = ScriptStore::new(vec![
        page(keys[..1000].to_vec(), Some("page2")),
        page(keys[1000..].to_vec(), Some("more")),
    ]);
    let sample = census::observe(&store, &config, "census", 8, 1280, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(sample.retained, 1280);
    let requests = store.lists.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].page_size(), 1000);
    assert_eq!(requests[1].page_size(), 281);
}

#[rstest::rstest]
#[case("short")]
#[case("empty-truncated")]
#[case("malformed")]
#[case("unordered")]
#[case("duplicate")]
#[tokio::test(start_paused = true)]
async fn unusable_pages_do_not_buy_replacement_requests(#[case] mode: &str) {
    let policy = config_for(15, "one");
    let first = census::report_key(&policy, "census", 8, 1, &Cutoff(None));
    let second = census::report_key(&config_for(15, "two"), "census", 8, 2, &Cutoff(None));
    let duplicate = census::report_key(&policy, "census", 8, 2, &Cutoff(None));
    let page = match mode {
        "short" => page(vec![first], Some("next")),
        "empty-truncated" => page(vec![], Some("next")),
        "malformed" => page(
            vec![format!("{}nonsense", policy.report_prefix("census", 8))],
            None,
        ),
        "unordered" => page(vec![second, first], None),
        "duplicate" => page(vec![first, duplicate], None),
        _ => unreachable!(),
    };
    let store = ScriptStore::new(vec![page]);
    let error = census::observe(&store, &policy, "census", 8, 480, Duration::from_secs(3))
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind(),
        StoreErrorKind::InvalidResponse { .. }
    ));
    assert_eq!(store.lists.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unusable_census_does_not_skip_the_profiler_channel_or_reset_source_age() {
    let policy = config_for(15, "leader");
    let store = Arc::new(ScriptStore::new(vec![
        page(vec![], Some("unfilled")),
        page(vec![], None),
    ]));
    let mut roster = Roster::initial(&policy, &policy.invocation_id);
    roster.population = Some(Population {
        window: 7,
        estimate: 1_000_000.0,
        computed_ms: 520_000,
    });
    roster.control.reporting = Cutoff::threshold(SPACE / 2000);
    let version = publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let observed = Observed {
        roster: roster.clone(),
        version,
        modified_at: store.now(),
    };
    let mut node = Worker::new(
        policy.clone(),
        store.clone(),
        store.clone(),
        Some(observed.clone()),
        watch::channel(false).0,
        true,
    )
    .unwrap();
    node.leader = Some(observed);
    node.leader_pass().await.unwrap();
    assert_eq!(store.lists.lock().unwrap().len(), 2);
    let roster = &node.leader.as_ref().unwrap().roster;
    assert_eq!(roster.population.as_ref().unwrap().window, 7);
    assert_eq!(roster.control.reporting.value(), SPACE / 2000);
    assert!(roster.next_control.is_none());
}

#[tokio::test(start_paused = true)]
async fn verified_empty_windows_ramp_future_cutoffs_and_never_publish_population_zero() {
    let policy = config_for(15, "leader");
    let mut roster = Roster::initial(&policy, &policy.invocation_id);
    roster.control.reporting = Cutoff::threshold(SPACE / 1000);
    roster.population = Some(Population {
        window: 7,
        estimate: 1_000_000.0,
        computed_ms: 520_000,
    });
    update(
        &mut roster,
        &policy,
        8,
        562_000,
        Ok(census::Sample {
            estimate: 0.0,
            retained: 0,
        }),
        Ok(census::Sample {
            estimate: 0.0,
            retained: 0,
        }),
    );
    let next = roster.next_control.as_ref().unwrap();
    assert_eq!(next.reporting.value(), (SPACE / 1000) * 2);
    assert_eq!(next.effective_window, 10);
    assert_eq!(
        roster.control.reporting.value(),
        SPACE / 1000,
        "already-open lottery stays frozen"
    );
    assert_eq!(roster.population.as_ref().unwrap().estimate, 1_000_000.0);
    roster.promote(9);
    assert_eq!(roster.control.reporting.value(), SPACE / 1000);
    roster.promote(10);
    assert_eq!(roster.control.reporting.value(), (SPACE / 1000) * 2);
}

#[tokio::test(start_paused = true)]
async fn any_disabled_process_can_claim_a_successfully_observed_expired_lease() {
    let store = store_at(600_000);
    let policy = config_for(15, "quiet");
    let mut roster = Roster::initial(&policy, &nonce());
    roster.control.reporting = Cutoff::threshold(1);
    publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let mut node = worker(&policy, store.clone(), false);
    let observed = node.read_roster().await.unwrap();
    node.learn(observed, node.now().unwrap());
    assert!(!node.election_needed);
    tokio::time::advance(Duration::from_secs(186)).await;
    let observed = node.read_roster().await.unwrap();
    node.learn(observed, node.now().unwrap());
    assert!(node.election_needed);
    node.election().await.unwrap();
    assert!(node.leader.is_some());
    assert!(!*node.signal.borrow());
}

#[tokio::test(start_paused = true)]
async fn uncertain_claim_reconciles_the_same_roster_revision_and_avoids_blind_retry() {
    let store = store_at(600_000);
    let policy = config_for(15, "leader");
    let mut node = worker(&policy, store.clone(), false);
    node.election_needed = true;
    store.lose_write_acknowledgements(1);
    node.election().await.unwrap();
    let observed = node.leader.as_ref().unwrap();
    assert_eq!(observed.roster.holder, policy.invocation_id);
    assert_eq!(store.objects().len(), 1);
    let persisted = decode(
        store
            .get(format!("{}roster.json", policy.pool_prefix()))
            .await
            .unwrap()
            .unwrap(),
        &policy,
    )
    .unwrap();
    assert_eq!(persisted.roster.revision, observed.roster.revision);
}

#[tokio::test(start_paused = true)]
async fn reservation_survives_takeover_and_a_stale_holder_cannot_publish() {
    let store = store_at(622_000);
    let old = config_for(15, "old");
    let new = config_for(15, "new");
    let mut roster = Roster::initial(&old, &old.invocation_id);
    roster.last_window = Some(9);
    let old_version = publish(store.as_ref(), &old, &roster, WriteCondition::absent()).await;
    let mut replacement = roster.clone();
    replacement.mutate(&new.invocation_id).unwrap();
    let new_version = publish(
        store.as_ref(),
        &new,
        &replacement,
        WriteCondition::matching(old_version.clone()),
    )
    .await;
    roster.mutate(&old.invocation_id).unwrap();
    assert!(
        replace(
            &old,
            store.as_ref(),
            store.as_ref(),
            &roster,
            Some(&old_version)
        )
        .await
        .unwrap()
        .is_none()
    );
    let observed = Observed {
        roster: replacement,
        version: new_version,
        modified_at: store.now(),
    };
    let mut leader = worker(&new, store.clone(), false);
    leader.leader = Some(observed);
    let before = store.operation_count();
    leader.leader_pass().await.unwrap();
    assert_eq!(
        store.operation_count() - before,
        2,
        "takeover renews but does not repeat reserved window LISTs"
    );
}

#[tokio::test(start_paused = true)]
async fn trim_has_poll_propagation_lead_and_cannot_repeat_before_postactivation_counts() {
    let policy = config_for(15, "leader");
    let mut roster = Roster::initial(&policy, &policy.invocation_id);
    roster.epoch = 1;
    update(
        &mut roster,
        &policy,
        8,
        562_000,
        Ok(census::Sample {
            estimate: 1000.0,
            retained: 480,
        }),
        Ok(census::Sample {
            estimate: 100.0,
            retained: 64,
        }),
    );
    let first = roster.trim.clone().unwrap();
    assert!(first.effective_window * 60_000 > 562_000 + 60_000 + 10_000 + 9000 + 2000);
    assert!((first.survival.probability() - 0.15).abs() < 1e-12);
    update(
        &mut roster,
        &policy,
        first.effective_window,
        1_300_000,
        Ok(census::Sample {
            estimate: 1000.0,
            retained: 480,
        }),
        Ok(census::Sample {
            estimate: 100.0,
            retained: 64,
        }),
    );
    assert_eq!(roster.trim.as_ref().unwrap().salt, first.salt);
}

#[tokio::test(start_paused = true)]
async fn trim_lead_reserves_slow_publication_and_uncertain_write_readback() {
    let store = store_at(638_999);
    let policy = config_for(15, "slow-publication");
    let mut roster = Roster::initial(&policy, &policy.invocation_id);
    let previous = publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    roster.mutate(&policy.invocation_id).unwrap();
    update(
        &mut roster,
        &policy,
        9,
        638_999,
        Ok(census::Sample {
            estimate: 1000.0,
            retained: 480,
        }),
        Ok(census::Sample {
            estimate: 100.0,
            retained: 64,
        }),
    );
    let activation = roster.trim.as_ref().unwrap().effective_window * 60_000;
    assert_eq!(activation, 720_000, "pin the smallest propagation margin");
    store.set_latency(Duration::from_millis(2999));
    store.lose_write_acknowledgements(1);
    let published = replace(
        &policy,
        store.as_ref(),
        store.as_ref(),
        &roster,
        Some(&previous),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(millis(published.modified_at).unwrap(), 641_998);
    assert_eq!(millis(store.now()).unwrap(), 644_997);
    assert!(activation - millis(store.now()).unwrap() > 75_000);
}

#[tokio::test(start_paused = true)]
async fn a_role_read_of_an_incompatible_policy_refuses_further_joining() {
    let store = store_at(600_000);
    let policy = config_for(15, "incompatible-role");
    let mut foreign = Roster::initial(&policy, &nonce());
    foreign.policy.target = 16;
    publish(store.as_ref(), &policy, &foreign, WriteCondition::absent()).await;
    let node = worker(&policy, store, true);
    let error = node.read_roster().await.err().unwrap();
    assert!(matches!(error.kind(), StoreErrorKind::Unsupported {}));
    assert!(!node.cache.borrow().compatible);
    assert!(
        *node.signal.borrow(),
        "protocol refusal is separate from recorder state"
    );
}

#[tokio::test(start_paused = true)]
async fn uncertain_roster_publication_refuses_an_incompatible_superseding_readback() {
    struct SupersedingStore {
        inner: MemoryStore,
        foreign: Bytes,
    }
    impl Clock for SupersedingStore {
        fn now(&self) -> SystemTime {
            self.inner.now()
        }
    }
    impl SamplingStore for SupersedingStore {
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            self.inner.get(key)
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            Box::pin(async move {
                let key = request.key().to_owned();
                let own = self.inner.put(request).await?;
                assert!(matches!(own, PutOutcome::Stored { .. }));
                self.inner
                    .put(
                        PutRequest::builder()
                            .key(key)
                            .body(self.foreign.clone())
                            .condition(WriteCondition::unconditional())
                            .build(),
                    )
                    .await?;
                Err(StoreError::new(
                    StoreErrorKind::write_outcome_unknown(),
                    "own committed CAS was superseded before readback",
                ))
            })
        }
        fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.inner.list_sorted_page(request)
        }
    }
    let policy = config_for(15, "incompatible-readback");
    let own = Roster::initial(&policy, &policy.invocation_id);
    let mut foreign = own.clone();
    foreign.policy.target = 16;
    foreign.mutate(&nonce()).unwrap();
    let store = Arc::new(SupersedingStore {
        inner: MemoryStore::at(UNIX_EPOCH + Duration::from_secs(600)),
        foreign: serialize(&foreign),
    });
    let node = Worker::new(
        policy,
        store.clone(),
        store,
        None,
        watch::channel(true).0,
        true,
    )
    .unwrap();
    let error = node.replace(&own, None).await.err().unwrap();
    assert!(matches!(error.kind(), StoreErrorKind::Unsupported {}));
    assert!(!node.cache.borrow().compatible);
    assert!(*node.signal.borrow());
}

#[test]
fn delayed_cache_responses_cannot_restore_old_controls_or_known_incompatibility() {
    let policy = config_for(15, "cache");
    let mut old = Roster::initial(&policy, &nonce());
    old.epoch = 10;
    let older = Observed {
        roster: old,
        version: VersionToken::new("old"),
        modified_at: UNIX_EPOCH + Duration::from_secs(600),
    };
    let mut new = older.clone();
    new.roster.mutate(&policy.invocation_id).unwrap();
    new.version = VersionToken::new("new");
    new.roster.next_control = Some(Control {
        source_window: Some(9),
        effective_window: 11,
        reporting: Cutoff::threshold(SPACE / 2),
    });
    let (cache, _) = watch::channel(Cache {
        observed: Some(older.clone()),
        compatible: true,
    });
    publish_cache(&cache, Some(new.clone()), true);
    publish_cache(&cache, Some(older), true);
    assert_eq!(
        cache.borrow().observed.as_ref().unwrap().version,
        new.version
    );
    publish_cache(&cache, None, false);
    // Even a delayed response for the same accepted epoch/revision must not undo
    // a successful incompatible response that completed before it.
    publish_cache(&cache, Some(new), true);
    assert!(!cache.borrow().compatible);
    assert_eq!(cache.borrow().observed.as_ref().unwrap().roster.epoch, 11);
}

#[tokio::test(start_paused = true)]
async fn cancelled_session_preserves_immutable_historical_reports() {
    let store = store_at(600_000);
    let policy = config_for(15, "node");
    let key = census::report_key(&policy, "census", 9, 1, &Cutoff(None));
    report(store.as_ref(), &policy, "census", 9, 1, Cutoff(None)).await;
    let prepared =
        prepare_using(policy, store.clone(), Duration::from_secs(1), store.clone()).await;
    prepared
        .into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    assert!(store.objects().contains_key(&key));
}

#[tokio::test(start_paused = true)]
async fn slow_leader_requests_do_not_block_either_observation_channel() {
    let inner = store_at(600_000);
    let policy = config_for(15, "overlap-48");
    let (puts, mut events) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(NotifyingStore {
        inner: inner.clone(),
        puts,
        active: AtomicUsize::new(0),
    });
    let prepared = prepare_using(
        policy.clone(),
        store.clone(),
        Duration::from_secs(1),
        inner.clone(),
    )
    .await;
    assert!(prepared.enabled);
    let session = prepared.into_session().unwrap();
    inner.set_latency(Duration::from_millis(2500));
    let census_key = census::report_key(
        &policy,
        "census",
        10,
        policy.draw("census-rank", "10"),
        &Cutoff(None),
    );
    let profiler_key = census::report_key(
        &policy,
        "profilers",
        10,
        policy.draw("profiler-rank", "10"),
        &Cutoff(None),
    );
    tokio::time::timeout(Duration::from_secs(40), async {
        let (mut census, mut profiler) = (false, false);
        while !(census && profiler) {
            let (key, completed) = events.recv().await.unwrap();
            census |= completed && key == census_key;
            profiler |= completed && key == profiler_key;
        }
    })
    .await
    .expect("both independent observation jobs must finish in their original window");
    let log = inner.operation_log();
    for (key, purpose, get_delay) in [
        (&census_key, "census-instant", 2500),
        (&profiler_key, "profiler-instant", 0),
    ] {
        let instant = 600_000 + (u128::from(policy.draw(purpose, "10")) * 60_000 / SPACE) as u64;
        let put_ms = log
            .iter()
            .find_map(|(at, operation)| match operation {
                StoreOperation::Put { key: actual, .. } if actual == key => {
                    Some(millis(*at).unwrap())
                }
                _ => None,
            })
            .expect("the exact rank and cutoff key was written");
        assert!(
            (instant + get_delay..=instant + get_delay + 1).contains(&put_ms),
            "{purpose} at {instant} was postponed to {put_ms}"
        );
        assert!(
            log.iter().any(|(at, operation)| {
                matches!(
                    operation,
                    StoreOperation::Get(_) | StoreOperation::Put { .. }
                ) && millis(*at).unwrap() <= instant
                    && millis(*at).unwrap() + 2500 > instant
            }),
            "the observation did not overlap another storage request"
        );
        assert!(inner.objects().contains_key(key));
    }
    session.shutdown(Duration::from_secs(1)).await;
    assert_eq!(store.active.load(Ordering::SeqCst), 0);
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn a_fast_leaders_promoted_control_waits_for_the_local_future_boundary(
    #[case] present_at_startup: bool,
) {
    let inner = store_at(600_000);
    let policy = config_for(15, "overlap-48");
    let future = Control {
        source_window: Some(9),
        effective_window: 11,
        reporting: Cutoff::threshold(1),
    };
    let mut roster = Roster::initial(&policy, &nonce());
    roster.epoch = 1;
    if present_at_startup {
        roster.control = future.clone();
    }
    roster.population = Some(Population {
        window: 8,
        estimate: 15.0,
        computed_ms: 562_000,
    });
    let previous = publish(inner.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let (puts, mut events) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(NotifyingStore {
        inner: inner.clone(),
        puts,
        active: AtomicUsize::new(0),
    });
    let prepared =
        prepare_using(policy.clone(), store, Duration::from_secs(1), inner.clone()).await;
    assert!(prepared.enabled);
    let session = prepared.into_session().unwrap();
    if !present_at_startup {
        sleep_until(&inner, 605_000).await;
        let incumbent = roster.holder.clone();
        roster.mutate(&incumbent).unwrap();
        // A faster leader has promoted this control; the local clock is still
        // in window 10, with an already-frozen full-support opportunity.
        roster.control = future;
        publish(
            inner.as_ref(),
            &policy,
            &roster,
            WriteCondition::matching(previous),
        )
        .await;
    }
    let frozen = census::report_key(
        &policy,
        "census",
        10,
        policy.draw("census-rank", "10"),
        &Cutoff(None),
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (key, completed) = events.recv().await.unwrap();
            if completed && key == frozen {
                break;
            }
        }
    })
    .await
    .expect("a current-window control GET changed the frozen cutoff");
    assert!(inner.objects().contains_key(&frozen));
    assert!(policy.draw("census-rank", "11") >= 1);
    let next_instant =
        660_000 + (u128::from(policy.draw("census-instant", "11")) * 60_000 / SPACE) as u64;
    sleep_until(&inner, next_instant + 1).await;
    assert!(
        !inner.operation_log().iter().any(|(_, op)| {
            matches!(op, StoreOperation::Put { key, .. } if key.starts_with(&policy.report_prefix("census", 11)))
        }),
        "the future cutoff did not become effective at its own boundary"
    );
    assert!(*session.recording_changes().borrow());
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn forced_shutdown_joins_pending_report_and_controller_requests() {
    let inner = store_at(600_000);
    let policy = config_for(15, "overlap-48");
    let historical = census::report_key(&policy, "census", 9, 1, &Cutoff(None));
    report(inner.as_ref(), &policy, "census", 9, 1, Cutoff(None)).await;
    let (puts, mut events) = tokio::sync::mpsc::unbounded_channel();
    let store = Arc::new(NotifyingStore {
        inner: inner.clone(),
        puts,
        active: AtomicUsize::new(0),
    });
    let session = prepare_using(
        policy.clone(),
        store.clone(),
        Duration::from_secs(1),
        inner.clone(),
    )
    .await
    .into_session()
    .unwrap();
    let weights = session.weights().unwrap();
    let recording = session.recording_changes();
    inner.set_latency(Duration::from_millis(2500));
    let pending = census::report_key(
        &policy,
        "profilers",
        10,
        policy.draw("profiler-rank", "10"),
        &Cutoff(None),
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (key, completed) = events.recv().await.unwrap();
            if key == pending && !completed {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(store.active.load(Ordering::SeqCst) >= 2);
    session.shutdown(Duration::ZERO).await;
    assert_eq!(
        store.active.load(Ordering::SeqCst),
        0,
        "shutdown returned while storage futures still belonged to the session"
    );
    assert!(!*recording.borrow());
    assert_eq!(metadata_weight(&weights), "0");
    assert!(inner.objects().contains_key(&historical));
    assert!(!inner.objects().contains_key(&pending));
    let attempts = inner.operation_count();
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(inner.operation_count(), attempts);
}

#[rstest::rstest]
#[case(0u128, false)]
#[case(1u128, true)]
#[tokio::test(start_paused = true)]
async fn common_trim_boundary_is_independent_of_a_slow_many_page_leader_round(
    #[case] salt: u128,
    #[case] survives: bool,
) {
    let store = store_at(480_000);
    let policy = config_for(400, "boundary-11");
    let mut roster = Roster::initial(&policy, &policy.invocation_id);
    roster.epoch = 1;
    roster.control.reporting = Cutoff::threshold(1);
    roster.population = Some(Population {
        window: 6,
        estimate: 400.0,
        computed_ms: 442_000,
    });
    publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let session = prepare_using(
        policy.clone(),
        store.clone(),
        Duration::from_secs(1),
        store.clone(),
    )
    .await
    .into_session()
    .unwrap();
    let weights = session.weights().unwrap();
    let recording = session.recording_changes();
    sleep_until(&store, 562_000).await;
    let observed = decode(
        store
            .get(format!("{}roster.json", policy.pool_prefix()))
            .await
            .unwrap()
            .unwrap(),
        &policy,
    )
    .unwrap();
    roster = observed.roster;
    roster.mutate(&policy.invocation_id).unwrap();
    roster.trim = Some(Trim {
        epoch: roster.epoch,
        salt: ulid::Ulid::from(salt).to_string(),
        source_window: 8,
        effective_window: 11,
        created_ms: 562_000,
        survival: Cutoff::threshold(SPACE / 2),
    });
    assert_eq!(
        roster
            .trim
            .as_ref()
            .unwrap()
            .survival
            .includes(policy.draw("trim", &roster.trim.as_ref().unwrap().salt)),
        survives
    );
    publish(
        store.as_ref(),
        &policy,
        &roster,
        WriteCondition::matching(observed.version),
    )
    .await;
    // Thirteen valid pages make the final healthy 2.5s CAS cross the boundary.
    for i in 0..12_801u64 {
        let mut member = policy.clone();
        member.invocation_id = ulid::Ulid::from(u128::from(i) + 1).to_string();
        report(store.as_ref(), &member, "census", 9, i, Cutoff(None)).await;
    }
    store.set_latency(Duration::from_millis(2500));
    sleep_until(&store, 659_999).await;
    assert_eq!(metadata_weight(&weights), "1");
    sleep_until(&store, 660_001).await;
    assert_eq!(
        metadata_weight(&weights),
        if survives { "2" } else { "0" },
        "leader storage work postponed the common thinning boundary"
    );
    assert_eq!(*recording.borrow(), survives);
    let log = store.operation_log();
    assert!(
        log.iter().any(|(at, operation)| {
            matches!(operation, StoreOperation::Put { key, body } if key.ends_with("/roster.json") && !body.is_empty())
                && millis(*at).unwrap() < 660_000
                && millis(*at).unwrap() + 2500 > 660_000
        }),
        "the leader's final CAS did not overlap the thinning boundary"
    );
    assert!(
        log.iter().any(|(at, operation)| {
            matches!(operation, StoreOperation::Get(_)) && millis(*at).unwrap() == 659_231
        }),
        "active control polls must run during that leader round"
    );
    sleep_until(&store, 680_001).await;
    assert_eq!(
        metadata_weight(&weights),
        if survives { "2" } else { "0" },
        "thinning applied twice or re-enabled an excluded recorder"
    );
    assert_eq!(*recording.borrow(), survives);
    session.shutdown(Duration::from_secs(1)).await;
}

#[rstest::rstest]
#[case(vec![("zone/name=%雪", "production/api=50%東京")], "zone%2Fname%3D%25雪=production%2Fapi%3D50%25東京/")]
#[case(vec![("z", "two"), ("a", "one")], "a=one/z=two/")]
#[test]
fn dimensions_remain_sorted_and_hive_escaped(
    #[case] dimensions: Vec<(&str, &str)>,
    #[case] expected: &str,
) {
    let mut config = config_for(15, "node");
    config.dimensions = dimensions
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    assert_eq!(config.pool_path(), expected);
}

#[tokio::test(start_paused = true)]
async fn report_put_failures_have_exactly_two_attempts_without_a_new_lottery() {
    let store = store_at(600_000);
    let policy = config_for(15, "partition");
    store.set_unavailable(true);
    let before = store.operation_count();
    assert!(
        observation::write_report(
            &policy,
            store.as_ref(),
            store.as_ref(),
            "census",
            10,
            1,
            &Cutoff(None)
        )
        .await
        .is_err()
    );
    assert_eq!(store.operation_count() - before, 2);
    assert!(store.objects().is_empty());
}

#[test]
fn positive_admission_keeps_one_rank_quantum_for_huge_valid_estimates() {
    let population = Population {
        window: 1,
        estimate: (SPACE as f64) * 480.0,
        computed_ms: 142_000,
    };
    assert_eq!(population.admission(1).value(), 1);
    assert_eq!(population.admission(0).value(), 0);
}

#[tokio::test(start_paused = true)]
async fn invalid_startup_clock_returns_the_configured_fallback_without_panicking() {
    struct InvalidClock;
    impl Clock for InvalidClock {
        fn now(&self) -> SystemTime {
            UNIX_EPOCH - Duration::from_secs(1)
        }
    }
    let store = store_at(600_000);
    let policy = config_for(15, "invalid-clock");
    let prepared = prepare_using(
        policy,
        store,
        Duration::from_secs(1),
        Arc::new(InvalidClock),
    )
    .await;
    assert!(prepared.enabled);
    assert!(matches!(
        prepared.reason,
        DecisionReason::StoreFailure { .. }
    ));
    assert!(prepared.session.is_none());
}

#[tokio::test(start_paused = true)]
async fn report_deadline_uses_one_time_snapshot_and_cannot_underflow_on_clock_advance() {
    struct AdvancingClock(std::sync::atomic::AtomicU64);
    impl Clock for AdvancingClock {
        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_millis(self.0.fetch_add(2, Ordering::SeqCst))
        }
    }
    let store = store_at(600_000);
    let policy = config_for(15, "deadline");
    let clock = Arc::new(AdvancingClock(std::sync::atomic::AtomicU64::new(600_000)));
    clock.0.store(670_999, Ordering::SeqCst);
    observation::write_report(
        &policy,
        store.as_ref(),
        clock.as_ref(),
        "census",
        10,
        1,
        &Cutoff(None),
    )
    .await
    .unwrap();
    assert_eq!(store.objects().len(), 1);
    let before = store.operation_count();
    assert!(
        observation::write_report(
            &policy,
            store.as_ref(),
            clock.as_ref(),
            "census",
            10,
            2,
            &Cutoff(None)
        )
        .await
        .is_err()
    );
    assert_eq!(
        store.operation_count(),
        before,
        "expired report makes zero backend attempts"
    );
}

#[tokio::test(start_paused = true)]
async fn collection_waits_until_the_completed_window_closure_margin() {
    let store = store_at(621_999);
    let policy = config_for(15, "leader");
    let mut roster = Roster::initial(&policy, &policy.invocation_id);
    roster.last_window = Some(8);
    let version = publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    report(
        store.as_ref(),
        &config_for(15, "member"),
        "census",
        9,
        1,
        Cutoff(None),
    )
    .await;
    let observed = Observed {
        roster,
        version,
        modified_at: store.now(),
    };
    let mut node = worker(&policy, store.clone(), false);
    node.leader = Some(observed);
    let before = store.operation_count();
    node.leader_pass().await.unwrap();
    assert_eq!(
        store.operation_count() - before,
        2,
        "the current completed prefix is not yet closed"
    );
    assert!(node.leader.as_ref().unwrap().roster.population.is_none());
    tokio::time::advance(Duration::from_millis(1)).await;
    let before = store.operation_count();
    node.leader_pass().await.unwrap();
    assert_eq!(store.operation_count() - before, 5);
    assert_eq!(
        node.leader
            .as_ref()
            .unwrap()
            .roster
            .population
            .as_ref()
            .unwrap()
            .window,
        9
    );
}

#[tokio::test(start_paused = true)]
async fn slow_many_page_round_expires_with_a_reservation_and_no_false_population() {
    struct SlowStore {
        inner: MemoryStore,
        lists: AtomicUsize,
    }
    impl Clock for SlowStore {
        fn now(&self) -> SystemTime {
            self.inner.now()
        }
    }
    impl SamplingStore for SlowStore {
        fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
            self.inner.get(key)
        }
        fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
            self.inner.put(request)
        }
        fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if request.prefix().starts_with("v3/profilers/") {
                    return Ok(page(vec![], None));
                }
                let offset = request
                    .continuation()
                    .map_or(0, |cursor| cursor.as_str().parse::<usize>().unwrap());
                let next = offset + request.page_size();
                let keys = (offset..next)
                    .map(|i| {
                        format!(
                            "{}{:016x}.{}.10000000000000000",
                            request.prefix(),
                            i + 1,
                            ulid::Ulid::from(i as u128 + 1)
                        )
                    })
                    .collect();
                Ok(page(keys, Some(&next.to_string())))
            })
        }
    }
    let store = Arc::new(SlowStore {
        inner: MemoryStore::at(UNIX_EPOCH + Duration::from_millis(622_000)),
        lists: AtomicUsize::new(0),
    });
    let policy = config_for(10000, "slow-leader");
    let roster = Roster::initial(&policy, &policy.invocation_id);
    let version = publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let observed = Observed {
        roster,
        version,
        modified_at: store.now(),
    };
    let mut node = Worker::new(
        policy.clone(),
        store.clone(),
        store.clone(),
        Some(observed.clone()),
        watch::channel(false).0,
        true,
    )
    .unwrap();
    node.leader = Some(observed);
    let started = Instant::now();
    let error = node.leader_pass().await.unwrap_err();
    assert!(matches!(error.kind(), StoreErrorKind::Unavailable { .. }));
    assert_eq!(started.elapsed(), Duration::from_secs(60));
    let roster = decode(
        store
            .get(format!("{}roster.json", policy.pool_prefix()))
            .await
            .unwrap()
            .unwrap(),
        &policy,
    )
    .unwrap()
    .roster;
    assert_eq!(roster.last_window, Some(9));
    assert!(roster.population.is_none());
    let before = store.lists.load(Ordering::SeqCst);
    let observed = node.read_roster().await.unwrap();
    node.learn(observed, node.now().unwrap());
    assert!(
        node.leader.is_some(),
        "authoritative control reads resume after round cancellation"
    );
    assert_eq!(store.lists.load(Ordering::SeqCst), before);
}

#[rstest::rstest]
#[case("census")]
#[case("profilers")]
#[tokio::test(start_paused = true)]
async fn a_window_dimension_cannot_contaminate_another_pools_observation(#[case] channel: &str) {
    let store = store_at(622_000);
    let mut bare = config_for(15, "bare-pool");
    bare.dimensions.clear();
    let mut nested = config_for(15, "window-pool");
    nested.dimensions = vec![("window".into(), "0000000000000009".into())];
    bare.validate().unwrap();
    nested.validate().unwrap();
    report(store.as_ref(), &bare, channel, 9, 1, Cutoff(None)).await;
    report(store.as_ref(), &nested, channel, 9, 2, Cutoff(None)).await;
    for pool in [&bare, &nested] {
        let observed = census::observe(
            store.as_ref(),
            pool,
            channel,
            9,
            480,
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(observed.retained, 1);
        assert_eq!(observed.estimate, 1.0);
    }
}
