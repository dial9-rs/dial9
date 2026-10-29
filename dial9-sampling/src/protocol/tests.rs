use super::*;
use crate::{
    backend::{ContinuationToken, ListPage, ListRequest, ListedObject, StoreFuture},
    protocol::{Clock, prepare_using},
    test_store::MemoryStore,
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
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
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
async fn a_control_get_at_an_opportunity_changes_only_a_future_lottery() {
    let store = store_at(600_000);
    let policy = config_for(15, "node");
    let mut node = worker(&policy, store.clone(), false);
    node.cutoff = Cutoff::threshold(SPACE / 2);
    node.ensure_plan(600_000).unwrap();
    let old_rank = node.plan.as_ref().unwrap().census_rank;
    let mut roster = Roster::initial(&policy, &nonce());
    roster.control = Control {
        source_window: Some(8),
        effective_window: 9,
        reporting: Cutoff::threshold(SPACE / 4),
    };
    publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    node.poll().await.unwrap();
    assert_eq!(node.plan.as_ref().unwrap().cutoff.value(), SPACE / 2);
    assert_eq!(node.plan.as_ref().unwrap().census_rank, old_rank);
    tokio::time::advance(Duration::from_secs(60)).await;
    node.ensure_plan(660_000).unwrap();
    assert_eq!(node.plan.as_ref().unwrap().cutoff.value(), SPACE / 4);
    assert_ne!(node.plan.as_ref().unwrap().census_rank, old_rank);
}

#[tokio::test(start_paused = true)]
async fn get_failure_does_not_suppress_an_eligible_report_and_lost_put_ack_retries_same_key() {
    let policy = config_for(15, "node");
    let store = Arc::new(ScriptStore::new(vec![]));
    store.get_failure.store(true, Ordering::SeqCst);
    store.inner.lose_write_acknowledgements(1);
    let mut node = Worker::new(
        policy.clone(),
        store.clone(),
        store.clone(),
        None,
        watch::channel(false).0,
        false,
    )
    .unwrap();
    node.ensure_plan(600_000).unwrap();
    let at = node.plan.as_ref().unwrap().census_at;
    tokio::time::advance(Duration::from_millis(at - 600_000)).await;
    node.reports(at).await.unwrap();
    assert_eq!(store.puts.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.inner.objects().len(),
        1,
        "uncertain retry uses the same immutable object"
    );
    assert!(
        !node.election_needed,
        "failed GET cannot authorize election"
    );
    node.reports(at).await.unwrap();
    assert_eq!(
        store.puts.load(Ordering::SeqCst),
        2,
        "there is no second eligibility draw"
    );
}

#[tokio::test(start_paused = true)]
async fn late_start_and_suspension_skip_opportunities_without_catchup() {
    let store = store_at(600_000);
    let policy = config_for(15, "node");
    let mut first = worker(&policy, store.clone(), false);
    first.ensure_plan(600_000).unwrap();
    let at = first.plan.as_ref().unwrap().census_at;
    tokio::time::advance(Duration::from_millis(at - 600_000 + 1)).await;
    let mut late = worker(&policy, store.clone(), false);
    late.ensure_plan(at + 1).unwrap();
    late.reports(at + 1).await.unwrap();
    assert!(late.plan.as_ref().unwrap().census_done);
    assert!(store.objects().is_empty());
    first
        .reports(at + Timing::new(&policy).dispatch + 1)
        .await
        .unwrap();
    assert!(store.objects().is_empty());
}

#[tokio::test(start_paused = true)]
async fn polling_deadlines_are_fixed_phase_and_unrelated_gets_never_reset_them() {
    let store = store_at(600_000);
    let policy = config_for(15, "quiet");
    let mut node = worker(&policy, store.clone(), false);
    let deadline = node.next_poll_ms;
    assert!(deadline > 600_000 && deadline <= 900_000);
    let mut roster = Roster::initial(&policy, &nonce());
    roster.control.reporting = Cutoff::threshold(1);
    publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    for _ in 0..5 {
        node.poll().await.unwrap();
    }
    assert_eq!(node.next_poll_ms, deadline);
    let phase = policy.draw("roster-phase", "") % 300_000;
    assert_eq!(deadline % 300_000, phase);
}

#[tokio::test(start_paused = true)]
async fn any_disabled_process_can_claim_a_successfully_observed_expired_lease() {
    let store = store_at(600_000);
    let policy = config_for(15, "quiet");
    let mut roster = Roster::initial(&policy, &nonce());
    roster.control.reporting = Cutoff::threshold(1);
    publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let mut node = worker(&policy, store.clone(), false);
    node.poll().await.unwrap();
    assert!(!node.election_needed);
    tokio::time::advance(Duration::from_secs(186)).await;
    node.poll().await.unwrap();
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
    assert!(first.effective_window * 60_000 > 562_000 + 300_000 + 10_000 + 3000);
    assert!((first.survival.probability() - 0.15).abs() < 1e-12);
    update(
        &mut roster,
        &policy,
        first.effective_window + 5,
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

#[rstest::rstest]
#[case(true)]
#[case(false)]
#[tokio::test(start_paused = true)]
async fn future_trim_applies_once_and_maintains_inverse_survival_weight(#[case] survivor: bool) {
    let store = store_at(600_000);
    let policy = config_for(15, "node");
    let mut node = worker(&policy, store.clone(), true);
    let salt = (0..1000)
        .map(|i| ulid::Ulid::from(i).to_string())
        .find(|salt| (policy.draw("trim", salt) < u64::MAX / 2) == survivor)
        .unwrap();
    let trim = Trim {
        epoch: 1,
        salt,
        source_window: 9,
        effective_window: 11,
        created_ms: 600_000,
        survival: Cutoff::threshold(SPACE / 2),
    };
    let mut roster = Roster::initial(&policy, &nonce());
    roster.epoch = 1;
    roster.trim = Some(trim.clone());
    let version = publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    node.weights = Some(crate::trace_weights::TraceWeights::new(
        policy.pool_path(),
        10.0,
    ));
    node.observed = Some(Observed {
        roster,
        version,
        modified_at: store.now(),
    });
    node.pending_trim = Some(trim);
    node.apply_trim(10, 600_000).unwrap();
    assert_eq!(
        node.weights.as_ref().unwrap().current_expansion_weight(),
        Some(10.0)
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    node.apply_trim(11, 660_000).unwrap();
    node.apply_trim(11, 660_000).unwrap();
    assert_eq!(*node.signal.borrow(), survivor);
    assert_eq!(
        node.weights.as_ref().unwrap().current_expansion_weight(),
        Some(if survivor { 20.0 } else { 0.0 })
    );
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
    let node = worker(&policy, store.clone(), false);
    store.set_unavailable(true);
    let before = store.operation_count();
    assert!(
        node.write_report("census", 10, 1, &Cutoff(None))
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
    let node = Worker::new(
        policy,
        store.clone(),
        clock.clone(),
        None,
        watch::channel(false).0,
        false,
    )
    .unwrap();
    clock.0.store(670_999, Ordering::SeqCst);
    node.write_report("census", 10, 1, &Cutoff(None))
        .await
        .unwrap();
    assert_eq!(store.objects().len(), 1);
    let before = store.operation_count();
    assert!(
        node.write_report("census", 10, 2, &Cutoff(None))
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
async fn a_slow_startup_clock_never_uses_already_promoted_future_control_early() {
    let policy = config_for(15, "slow");
    let store = store_at(659_000);
    let mut roster = Roster::initial(&policy, &nonce());
    roster.control = Control {
        source_window: Some(9),
        effective_window: 11,
        reporting: Cutoff::threshold(SPACE / 100),
    };
    let version = publish(store.as_ref(), &policy, &roster, WriteCondition::absent()).await;
    let observed = Observed {
        roster,
        version,
        modified_at: store.now(),
    };
    let mut node = Worker::new(
        policy,
        store.clone(),
        store,
        Some(observed),
        watch::channel(false).0,
        true,
    )
    .unwrap();
    node.ensure_plan(659_000).unwrap();
    assert_eq!(
        node.plan.as_ref().unwrap().cutoff.value(),
        SPACE,
        "bootstrap fallback is positive and independent"
    );
}
