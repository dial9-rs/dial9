//! Concurrent regressions for the protocol's per-key storage assumptions.

use crate::{
    backend::{
        PutOutcome, PutRequest, SamplingStore, StoreErrorKind, VersionToken, WriteCondition,
    },
    test_store::MemoryStore,
};
use shuttle::{future::block_on, sync::mpsc, thread};
use std::sync::Arc;

const ROSTER: &str = "v3/pools/pool/roster.json";

fn put(store: &MemoryStore, key: &str, body: &str, condition: WriteCondition) -> VersionToken {
    match block_on(
        store.put(
            PutRequest::builder()
                .key(key)
                .body(body.to_owned().into())
                .condition(condition)
                .build(),
        ),
    )
    .unwrap()
    {
        PutOutcome::Stored { version, .. } => version,
        other => panic!("expected successful write: {other:?}"),
    }
}

dial9_core::shuttle_test! {
    default;
    fn shuttle_takeover_fences_a_paused_roster_publication() {
        let store = Arc::new(MemoryStore::new());
        let first = put(&store, ROSTER, "owner=A,revision=A1", WriteCondition::absent());
        let paused_store = store.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let paused = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            block_on(paused_store.put(
                PutRequest::builder()
                    .key(ROSTER)
                    .body("owner=A,revision=A2".into())
                    .condition(WriteCondition::matching(first))
                    .build(),
            )).unwrap()
        });
        ready_rx.recv().unwrap();
        let observed = block_on(store.get(ROSTER.into())).unwrap().unwrap();
        put(
            &store,
            ROSTER,
            "owner=B,revision=B1",
            WriteCondition::matching(observed.version().clone()),
        );
        resume_tx.send(()).unwrap();
        assert!(matches!(paused.join().unwrap(), PutOutcome::PreconditionFailed { .. }));
        assert_eq!(
            block_on(store.get(ROSTER.into())).unwrap().unwrap().body().as_ref(),
            b"owner=B,revision=B1",
        );
    }
}

dial9_core::shuttle_test! {
    default;
    fn shuttle_fresh_revision_prevents_body_etag_aba() {
        let store = Arc::new(MemoryStore::new());
        let first = put(&store, ROSTER, "owner=A,revision=A1", WriteCondition::absent());
        let stale = first.clone();
        let second = put(
            &store,
            ROSTER,
            "owner=B,revision=B1",
            WriteCondition::matching(first),
        );
        // Ownership returns to A, while the unique revision keeps the old
        // body-based ETag from becoming valid again.
        let third = put(
            &store,
            ROSTER,
            "owner=A,revision=A2",
            WriteCondition::matching(second),
        );
        assert_ne!(stale, third);
        let contender = store.clone();
        let publication = thread::spawn(move || {
            block_on(contender.put(
                PutRequest::builder()
                    .key(ROSTER)
                    .body("owner=A,revision=stale".into())
                    .condition(WriteCondition::matching(stale))
                    .build(),
            )).unwrap()
        });
        // Keep a storage operation runnable beside the stale publication so
        // PCT explores their lock ordering rather than a spawn followed by join.
        let observed = block_on(store.get(ROSTER.into())).unwrap().unwrap();
        assert_eq!(observed.version(), &third);
        assert!(matches!(publication.join().unwrap(), PutOutcome::PreconditionFailed { .. }));
        assert_eq!(
            block_on(store.get(ROSTER.into())).unwrap().unwrap().version(),
            &third,
        );
    }
}

dial9_core::shuttle_test! {
    default;
    fn shuttle_unknown_acknowledgement_does_not_admit_two_claimants() {
        let store = Arc::new(MemoryStore::new());
        store.lose_write_acknowledgements(1);
        let claim = |body: &'static str| {
            let store = store.clone();
            thread::spawn(move || {
                let result = block_on(store.put(
                    PutRequest::builder()
                        .key(ROSTER)
                        .body(body.into())
                        .condition(WriteCondition::absent())
                        .build(),
                ));
                match result {
                    Err(error) => {
                        assert!(matches!(error.kind(), StoreErrorKind::WriteOutcomeUnknown { .. }));
                        // Reconcile the committed value rather than retrying
                        // the old condition or claiming an unrelated object.
                        let committed = block_on(store.get(ROSTER.into())).unwrap().unwrap();
                        assert_eq!(committed.body().as_ref(), body.as_bytes());
                        true
                    }
                    Ok(PutOutcome::PreconditionFailed { .. }) => false,
                    other => panic!("unexpected claim outcome: {other:?}"),
                }
            })
        };
        let first = claim("owner=A,revision=A1");
        let second = claim("owner=B,revision=B1");
        assert_ne!(first.join().unwrap(), second.join().unwrap());
        assert_eq!(store.objects().len(), 1);
    }
}

dial9_core::shuttle_test! {
    default;
    fn shuttle_immutable_window_reports_survive_process_exit() {
        let store = Arc::new(MemoryStore::new());
        let first_key = "v3/census/pool/window=0000000000000001/0123456789abcdef.old-run.10000000000000000";
        let second_key = "v3/census/pool/window=0000000000000001/0123456789abcdef.new-run.10000000000000000";
        let first = store.clone();
        let old_run = thread::spawn(move || {
            put(&first, first_key, "", WriteCondition::absent());
        });
        let second = store.clone();
        let new_run = thread::spawn(move || {
            put(&second, second_key, "", WriteCondition::absent());
        });
        old_run.join().unwrap(); new_run.join().unwrap();
        assert!(block_on(store.get(first_key.into())).unwrap().is_some());
        assert!(block_on(store.get(second_key.into())).unwrap().is_some());
        assert_eq!(store.objects().len(), 2);
    }
}
