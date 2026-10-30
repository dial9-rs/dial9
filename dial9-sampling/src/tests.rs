use super::*;
use crate::{
    backend::*,
    test_store::{MemoryStore, StoreOperation},
};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[rstest::rstest]
#[case(None, 2000, 0, StartupFailure::disabled(), false)]
#[case(Some(500), 5000, 0, StartupFailure::disabled(), false)]
#[case(Some(500), 5000, 0, StartupFailure::assume_population(1), true)]
#[case(Some(500), 400, 150, StartupFailure::disabled(), false)]
#[tokio::test(start_paused = true)]
async fn startup_budget_covers_initialization_and_get_without_revisiting_fallback(
    #[case] timeout_ms: Option<u64>,
    #[case] initialization_ms: u64,
    #[case] storage_ms: u64,
    #[case] fallback: StartupFailure,
    #[case] expected: bool,
) {
    let store = Arc::new(MemoryStore::new());
    store.set_latency(Duration::from_millis(storage_ms));
    let sampling = Sampling::builder()
        .add_dimension("test", "timeout")
        .num_target_profilers(1)
        .startup_timeout(Duration::from_millis(timeout_ms.unwrap_or(1000)))
        .if_failed(fallback)
        .build();
    let before = tokio::time::Instant::now();
    // Exercise the shared admission deadline on a paused executor; the public
    // synchronous entry point owns a multi-thread runtime that cannot be paused.
    let prepared = Sampling::admit(
        &sampling.policy(),
        async {
            tokio::time::sleep(Duration::from_millis(initialization_ms)).await;
            Ok(store.clone())
        },
        sampling.startup_timeout,
    )
    .await;
    assert_eq!(
        before.elapsed(),
        Duration::from_millis(timeout_ms.unwrap_or(1000))
    );
    assert_eq!(prepared.enabled, expected);
    assert_eq!(prepared.reason, protocol::DecisionReason::Timeout);
    if let Some(session) = prepared.into_session() {
        session.shutdown(Duration::from_secs(1)).await;
    }
}

const WINDOW: u64 = 1000;
const SPACE: u128 = 1u128 << 64;

struct StoreClock(Arc<MemoryStore>);
impl protocol::Clock for StoreClock {
    fn now(&self) -> SystemTime {
        self.0.now()
    }
}

fn config(run: u128, target: u32) -> Sampling {
    let mut config = Sampling::builder()
        .num_target_profilers(target)
        .if_failed(StartupFailure::disabled())
        .build();
    config.invocation_id = ulid::Ulid::from(run).to_string();
    config
}
async fn tick(seconds: u64) {
    for _ in 0..seconds {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }
}
async fn prepare(
    config: &Sampling,
    store: Arc<dyn SamplingStore>,
    clock: Arc<MemoryStore>,
) -> protocol::PreparedSampling {
    protocol::prepare_using(
        config.clone(),
        store,
        Duration::from_secs(1),
        Arc::new(StoreClock(clock)),
    )
    .await
}
fn roster(store: &MemoryStore) -> Value {
    let object = store
        .objects()
        .into_iter()
        .find(|(key, _)| key.ends_with("/roster.json"))
        .expect("published roster")
        .1;
    serde_json::from_slice(object.body()).unwrap()
}
async fn publish(store: &MemoryStore, value: &Value) {
    let result = store
        .put(
            PutRequest::builder()
                .key("v3/pools/roster.json")
                .body(serde_json::to_vec(value).unwrap().into())
                .condition(WriteCondition::unconditional())
                .build(),
        )
        .await
        .unwrap();
    assert!(matches!(result, PutOutcome::Stored { .. }), "{result:?}");
}
fn reports(store: &MemoryStore, run: &str) -> Vec<(SystemTime, String, Vec<u8>)> {
    store
        .operation_log()
        .into_iter()
        .filter_map(|(at, operation)| match operation {
            StoreOperation::Put { key, body }
                if key.starts_with("v3/census/") && key.contains(run) =>
            {
                Some((at, key, body))
            }
            _ => None,
        })
        .collect()
}

/// Models an incumbent renewing its unchanged body while allowing read failures
/// and underfilled pages independently of successful report writes.
#[derive(Debug)]
struct ObservedStore {
    inner: Arc<MemoryStore>,
    fresh_lease: AtomicBool,
    failed_reads: AtomicBool,
    short_pages: bool,
}
impl SamplingStore for ObservedStore {
    fn get(&self, key: String) -> StoreFuture<'_, Option<StoredObject>> {
        Box::pin(async move {
            let object = self.inner.get(key).await?;
            if self.failed_reads.load(Ordering::SeqCst) {
                return Err(StoreError::new(
                    StoreErrorKind::unavailable(),
                    "lost read response",
                ));
            }
            Ok(object.map(|object| {
                if self.fresh_lease.load(Ordering::SeqCst) {
                    StoredObject::builder()
                        .body(object.body().clone())
                        .version(object.version().clone())
                        .modified_at(self.inner.now())
                        .build()
                } else {
                    object
                }
            }))
        })
    }
    fn put(&self, request: PutRequest) -> StoreFuture<'_, PutOutcome> {
        self.inner.put(request)
    }
    fn list_sorted_page(&self, request: ListRequest) -> StoreFuture<'_, ListPage> {
        if self.short_pages {
            self.inner.list_sorted_page(
                ListRequest::builder()
                    .prefix(request.prefix())
                    .page_size(1)
                    .maybe_continuation(request.continuation().cloned())
                    .build(),
            )
        } else {
            self.inner.list_sorted_page(request)
        }
    }
}
async fn warm_store(
    target: u32,
    cutoff: Option<u64>,
    estimate: f64,
) -> (Arc<MemoryStore>, Arc<ObservedStore>, Value) {
    let inner = Arc::new(MemoryStore::at(
        UNIX_EPOCH + Duration::from_secs(WINDOW * 60),
    ));
    let initial = prepare(&config(9001, target), inner.clone(), inner.clone()).await;
    let session = initial.into_session().expect("bootstrap maintenance");
    tick(3).await;
    let mut value = roster(&inner);
    session.shutdown(Duration::from_secs(1)).await;
    value["control"] =
        json!({"source_window":WINDOW-1,"effective_window":WINDOW,"reporting":cutoff});
    value["next_control"] = Value::Null;
    value["population"] =
        json!({"window":WINDOW-1,"estimate":estimate,"computed_ms":WINDOW*60_000});
    publish(&inner, &value).await;
    let observed = Arc::new(ObservedStore {
        inner: inner.clone(),
        fresh_lease: AtomicBool::new(true),
        failed_reads: AtomicBool::new(false),
        short_pages: false,
    });
    (inner, observed, value)
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn quiet_disabled_process_polls_on_anchored_deadlines_and_can_take_over(
    #[case] failed_reads: bool,
) {
    let (inner, backend, _) = warm_store(15, Some(1), 1e12).await;
    let start = inner.operation_log().len();
    let config = config(1, 15);
    let admission = prepare(&config, backend.clone(), inner.clone()).await;
    assert!(!admission.enabled);
    let session = admission.into_session().expect("quiet maintenance");
    backend.failed_reads.store(failed_reads, Ordering::SeqCst);
    tick(610).await;
    let times: Vec<_> = inner.operation_log()[start..]
        .iter()
        .filter_map(|(at, operation)| match operation {
            StoreOperation::Get(key) if key.ends_with("/roster.json") => Some(*at),
            _ => None,
        })
        .collect();
    assert!(times.len() >= 3, "scheduled reads: {times:?}");
    for pair in times.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]).unwrap() <= Duration::from_secs(300),
            "read gap: {pair:?}"
        );
    }
    assert_eq!(reports(&inner, &config.invocation_id).len(), 0);
    backend.failed_reads.store(false, Ordering::SeqCst);
    backend.fresh_lease.store(false, Ordering::SeqCst);
    tick(302).await;
    assert_eq!(roster(&inner)["holder"], config.invocation_id);
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn failed_read_does_not_redraw_or_suppress_the_frozen_report() {
    let (inner, backend, _) = warm_store(15, None, 1e12).await;
    let config = config(1, 15);
    let admission = prepare(&config, backend.clone(), inner.clone()).await;
    assert!(!admission.enabled);
    let session = admission.into_session().unwrap();
    assert!(
        reports(&inner, &config.invocation_id).is_empty(),
        "startup must not write before its random opportunity"
    );
    backend.failed_reads.store(true, Ordering::SeqCst);
    inner.lose_write_acknowledgements(1);
    tick(180).await;
    let written = reports(&inner, &config.invocation_id);
    assert!(written.len() >= 3, "report attempts: {written:?}");
    assert_eq!(
        written[0].1, written[1].1,
        "uncertain PUT retries the same report"
    );
    let mut by_window =
        std::collections::BTreeMap::<String, std::collections::HashSet<String>>::new();
    for (_, key, body) in &written {
        assert!(body.is_empty(), "report body: {body:?}");
        assert!(key.ends_with(".10000000000000000"), "frozen cutoff: {key}");
        by_window
            .entry(key.split('/').nth(3).unwrap().into())
            .or_default()
            .insert(key.clone());
    }
    for (window, keys) in by_window {
        assert_eq!(keys.len(), 1, "redrawn report in {window}: {keys:?}");
    }
    let before = inner.objects();
    session.shutdown(Duration::from_secs(1)).await;
    for (_, key, _) in written {
        assert_eq!(
            inner.objects()[&key].version(),
            before[&key].version(),
            "shutdown changed {key}"
        );
    }
}

async fn fill_reports(store: &MemoryStore, target_window: u64, population: u64) {
    for channel in ["census", "profilers"] {
        for index in 1..=population {
            let rank = (u128::from(index) * SPACE / u128::from(population + 1)) as u64;
            let key = format!(
                "v3/{channel}/windows/window={target_window:016x}/{rank:016x}.{}.{SPACE:017x}",
                ulid::Ulid::from(u128::from(index))
            );
            let outcome = store
                .put(
                    PutRequest::builder()
                        .key(key)
                        .body(bytes::Bytes::new())
                        .build(),
                )
                .await
                .unwrap();
            assert!(matches!(outcome, PutOutcome::Stored { .. }), "{outcome:?}");
        }
    }
}
#[rstest::rstest]
#[case(15, 2, 546)]
#[case(64, 4, 2306)]
#[tokio::test(start_paused = true)]
async fn large_population_lists_only_the_bounded_lowest_rank_sample(
    #[case] target: u32,
    #[case] calls: usize,
    #[case] keys: usize,
) {
    let inner = Arc::new(MemoryStore::at(
        UNIX_EPOCH + Duration::from_secs((WINDOW - 1) * 60),
    ));
    fill_reports(&inner, WINDOW - 1, 10_000).await;
    tick(83).await;
    let before = inner.operation_log().len();
    let prepared = prepare(&config(50_000, target), inner.clone(), inner.clone()).await;
    let session = prepared.into_session().unwrap();
    tick(3).await;
    let pages: Vec<_> = inner
        .operation_log()
        .into_iter()
        .skip(before)
        .filter_map(|(_, operation)| match operation {
            StoreOperation::List {
                prefix,
                limit,
                continuation,
            } => Some((prefix, limit, continuation)),
            _ => None,
        })
        .collect();
    assert_eq!(pages.len(), calls, "LISTs: {pages:?}");
    assert_eq!(pages.iter().map(|(_, limit, _)| limit).sum::<usize>(), keys);
    assert!(
        pages.iter().all(|(_, limit, _)| *limit <= 1000),
        "LISTs: {pages:?}"
    );
    assert_eq!(
        pages
            .iter()
            .filter(|(_, _, cursor)| cursor.is_none())
            .count(),
        2
    );
    let estimated = roster(&inner)["population"]["estimate"].as_f64().unwrap();
    assert!(
        (9500.0..10500.0).contains(&estimated),
        "estimated population: {estimated}"
    );
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn short_truncated_pages_hit_the_cap_without_publishing_a_partial_count() {
    let inner = Arc::new(MemoryStore::at(
        UNIX_EPOCH + Duration::from_secs((WINDOW - 1) * 60),
    ));
    fill_reports(&inner, WINDOW - 1, 100).await;
    tick(83).await;
    let before = inner.operation_log().len();
    let backend = Arc::new(ObservedStore {
        inner: inner.clone(),
        fresh_lease: AtomicBool::new(false),
        failed_reads: AtomicBool::new(false),
        short_pages: true,
    });
    let session = prepare(&config(50_000, 15), backend, inner.clone())
        .await
        .into_session()
        .unwrap();
    tick(3).await;
    let pages: Vec<_> = inner
        .operation_log()
        .into_iter()
        .skip(before)
        .filter(|(_, operation)| matches!(operation, StoreOperation::List { .. }))
        .collect();
    assert_eq!(pages.len(), 2, "LISTs: {pages:?}");
    assert_eq!(roster(&inner)["population"], Value::Null);
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn zero_observations_preserve_the_prior_positive_estimate() {
    let (inner, backend, _) = warm_store(15, Some(1), 1e12).await;
    backend.fresh_lease.store(false, Ordering::SeqCst);
    tick(190).await;
    let session = prepare(&config(1, 15), backend, inner.clone())
        .await
        .into_session()
        .unwrap();
    tick(3).await;
    assert_eq!(roster(&inner)["population"]["estimate"], 1e12);
    assert!(
        roster(&inner)["next_control"]["reporting"]
            .as_u64()
            .unwrap()
            > 1
    );
    session.shutdown(Duration::from_secs(1)).await;
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test(start_paused = true)]
async fn missed_opportunities_are_skipped_without_birth_or_resume_catchup(#[case] born_late: bool) {
    let (reference, reference_backend, _) = warm_store(15, None, 1e12).await;
    let config = config(1, 15);
    let baseline = prepare(&config, reference_backend, reference.clone())
        .await
        .into_session()
        .unwrap();
    tick(60).await;
    let (opportunity, _, _) = reports(&reference, &config.invocation_id)
        .into_iter()
        .find(|(_, key, _)| key.contains(&format!("window={WINDOW:016x}")))
        .expect("reference opportunity");
    baseline.shutdown(Duration::from_secs(1)).await;
    let (inner, backend, _) = warm_store(15, None, 1e12).await;
    let skip_to = opportunity + Duration::from_secs(3);
    let session = if born_late {
        tokio::time::advance(skip_to.duration_since(inner.now()).unwrap()).await;
        prepare(&config, backend, inner.clone())
            .await
            .into_session()
            .unwrap()
    } else {
        let session = prepare(&config, backend, inner.clone())
            .await
            .into_session()
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(skip_to.duration_since(inner.now()).unwrap()).await;
        session
    };
    tick(65).await;
    let written = reports(&inner, &config.invocation_id);
    assert!(
        !written
            .iter()
            .any(|(_, key, _)| key.contains(&format!("window={WINDOW:016x}"))),
        "late report attempts: {written:?}"
    );
    assert!(
        written
            .iter()
            .any(|(_, key, _)| key.contains(&format!("window={:016x}", WINDOW + 1))),
        "future opportunities: {written:?}"
    );
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn scalar_metadata_weights_each_actual_survived_coin_once() {
    use dial9_core::source::Source;
    let (inner, backend, mut value) = warm_store(15, None, 30.0).await;
    let prepared = prepare(&config(4, 15), backend, inner.clone()).await;
    assert!(prepared.enabled, "fixed startup half-coin fixture");
    let session = prepared.into_session().unwrap();
    let metadata_weight = || {
        let mut source = session.weights().expect("enabled sampling Source");
        let mut metadata = Vec::new();
        source.segment_metadata(&mut metadata);
        metadata
            .into_iter()
            .find(|(key, _)| key == "dial9.sampling.weight")
            .unwrap()
            .1
    };
    assert_eq!(metadata_weight(), "2");
    tick(80).await;
    let epoch = value["epoch"].as_u64().unwrap() + 1;
    value["epoch"] = json!(epoch);
    value["trim"] = json!({"epoch":epoch,"salt":ulid::Ulid::from(1u128).to_string(),"source_window":WINDOW,"effective_window":WINDOW+7,"created_ms":(WINDOW+1)*60_000+23_000,"survival":1u64<<63});
    publish(&inner, &value).await;
    tick(340).await;
    assert_eq!(metadata_weight(), "4");
    tick(120).await;
    assert_eq!(
        metadata_weight(),
        "4",
        "re-reading the same trim must not multiply again"
    );
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn unrelated_report_reads_do_not_postpone_the_fixed_poll_deadline() {
    let (quiet, quiet_backend, _) = warm_store(15, Some(1), 1e12).await;
    let config = config(1, 15);
    let before = quiet.operation_log().len();
    let baseline = prepare(&config, quiet_backend, quiet.clone())
        .await
        .into_session()
        .unwrap();
    tick(305).await;
    let anchor = quiet
        .operation_log()
        .into_iter()
        .skip(before)
        .filter_map(|(at, operation)| match operation {
            StoreOperation::Get(_) => Some(at),
            _ => None,
        })
        .nth(1)
        .expect("first anchored read after startup");
    baseline.shutdown(Duration::from_secs(1)).await;

    let (inner, backend, _) = warm_store(15, None, 1e12).await;
    let before = inner.operation_log().len();
    let session = prepare(&config, backend, inner.clone())
        .await
        .into_session()
        .unwrap();
    tick(305).await;
    let reads: Vec<_> = inner
        .operation_log()
        .into_iter()
        .skip(before)
        .filter_map(|(at, operation)| match operation {
            StoreOperation::Get(_) => Some(at),
            _ => None,
        })
        .collect();
    assert!(
        reads.contains(&anchor),
        "report reads postponed {anchor:?}: {reads:?}"
    );
    assert!(
        reads
            .iter()
            .any(|at| *at < anchor && *at > UNIX_EPOCH + Duration::from_secs(WINDOW * 60 + 3)),
        "no earlier role read: {reads:?}"
    );
    session.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn completed_census_supplies_population_for_later_startup() {
    let inner = Arc::new(MemoryStore::at(
        UNIX_EPOCH + Duration::from_secs(WINDOW * 60),
    ));
    let cold = prepare(&config(1, 1), inner.clone(), inner.clone()).await;
    assert!(!cold.enabled, "cold start uses disabled fallback");
    let cold = cold.into_session().unwrap();
    tick(150).await;
    let value = roster(&inner);
    assert_eq!(value["population"]["estimate"], 1.0);
    assert!(value["population"]["window"].as_u64().unwrap() >= WINDOW);
    let warm = prepare(&config(2, 1), inner.clone(), inner.clone()).await;
    assert!(
        warm.enabled,
        "published population supports startup admission"
    );
    assert!(
        cold.weights().is_none(),
        "original startup remains disabled"
    );
    warm.into_session()
        .unwrap()
        .shutdown(Duration::from_secs(1))
        .await;
    cold.shutdown(Duration::from_secs(1)).await;
}

fn population_lag(store: &MemoryStore) -> u64 {
    let window = store.now().duration_since(UNIX_EPOCH).unwrap().as_secs() / 60;
    window - roster(store)["population"]["window"].as_u64().unwrap()
}

/// Replaces a running fleet one process per minute, as a rolling deploy does.
#[rstest::rstest]
#[case::same_target(4, 4)]
#[case::raised_target(4, 8)]
#[case::lowered_target(8, 2)]
#[tokio::test(start_paused = true)]
async fn rolling_deploy_keeps_one_coordinated_roster_across_target_changes(
    #[case] old_target: u32,
    #[case] new_target: u32,
) {
    const FLEET: u128 = 12;
    let store = Arc::new(MemoryStore::at(
        UNIX_EPOCH + Duration::from_secs(WINDOW * 60),
    ));
    let mut fleet = std::collections::VecDeque::new();
    for run in 1..=FLEET {
        let prepared = prepare(&config(run, old_target), store.clone(), store.clone()).await;
        fleet.push_back(prepared.into_session().expect("maintenance"));
    }
    tick(240).await;
    assert!(population_lag(&store) <= 2);
    for run in 1..=FLEET {
        fleet
            .pop_front()
            .unwrap()
            .shutdown(Duration::from_secs(9))
            .await;
        let config = config(1000 + run, new_target);
        let prepared = prepare(&config, store.clone(), store.clone()).await;
        assert_ne!(
            prepared.reason,
            protocol::DecisionReason::IncompatiblePolicy,
            "a target change must not lock new processes out"
        );
        fleet.push_back(prepared.into_session().expect("maintenance"));
        tick(60).await;
        // A departing leader hands off at once, so population never goes stale.
        assert!(
            population_lag(&store) <= 2,
            "stale population after replacing process {run}: {}",
            roster(&store)
        );
        let estimate = roster(&store)["population"]["estimate"].as_f64().unwrap();
        assert!(
            (10.0..=14.0).contains(&estimate),
            "estimate {estimate} for a fleet of {FLEET}"
        );
    }
    let holder: ulid::Ulid = roster(&store)["holder"].as_str().unwrap().parse().unwrap();
    assert!(u128::from(holder) > FLEET, "new configuration leads");
    for session in fleet {
        session.shutdown(Duration::from_secs(9)).await;
    }
}

/// An older release that uses a different roster schema keeps its lease while
/// any of its processes renew it; the new release claims the pool after that.
#[tokio::test(start_paused = true)]
async fn rolling_deploy_across_a_schema_change_claims_the_pool_once_the_old_fleet_exits() {
    let store = Arc::new(MemoryStore::at(
        UNIX_EPOCH + Duration::from_secs(WINDOW * 60),
    ));
    let old_roster = json!({"policy": {"schema": 1}, "holder": "old-release"});
    let mut fleet = Vec::new();
    for run in 1..=6 {
        // The old leader renews every minute while its fleet drains.
        publish(&store, &old_roster).await;
        let prepared = prepare(&config(run, 4), store.clone(), store.clone()).await;
        assert_eq!(
            prepared.reason,
            protocol::DecisionReason::IncompatiblePolicy
        );
        assert!(!prepared.enabled, "configured fallback applies");
        fleet.push(prepared.into_session().expect("maintenance"));
        tick(60).await;
        assert_eq!(roster(&store)["holder"], "old-release");
    }
    assert!(
        store
            .objects()
            .iter()
            .all(|(key, _)| key.ends_with("/roster.json")),
        "no reports under the old release's lease"
    );
    // Old fleet gone: the 186s lease lapses and a new process claims within a poll.
    tick(190).await;
    let holder: ulid::Ulid = roster(&store)["holder"].as_str().unwrap().parse().unwrap();
    assert!((1..=6).contains(&u128::from(holder)));
    tick(180).await;
    assert!(population_lag(&store) <= 2, "{}", roster(&store));
    let started = prepare(&config(7, 4), store.clone(), store.clone()).await;
    assert!(matches!(
        started.reason,
        protocol::DecisionReason::Roster | protocol::DecisionReason::OutsideCutoff
    ));
    fleet.push(started.into_session().unwrap());
    for session in fleet {
        session.shutdown(Duration::from_secs(9)).await;
    }
}
