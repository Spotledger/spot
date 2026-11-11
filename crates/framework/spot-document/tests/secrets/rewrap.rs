//! Key rotation: rewrap_secrets moves sealed values and blind indexes onto the current key in bounded, resumable batches.

use crate::support::*;

async fn rotation_rewraps_in_bounded_resumable_batches<B: StorageBackend + ChangeFeed>(r: &Rig<B>) {
    let access = r.access();
    let mut ids = Vec::new();
    for i in 0..5 {
        let (id, v) = r.account(&format!("r{i}")).await;
        let v = r
            .engine
            .set_secret(
                &access,
                "account",
                id,
                "token",
                s(&format!("{TOKEN}{i}")),
                v,
            )
            .await
            .unwrap();
        r.engine
            .set_secret(
                &access,
                "account",
                id,
                "password",
                s(&format!("{PASSWORD}{i}")),
                v,
            )
            .await
            .unwrap();
        ids.push(id);
    }
    // Nothing to do on a current key.
    let idle = r
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(100))
        .await
        .unwrap();
    assert_eq!(
        (
            idle.examined,
            idle.rewrapped,
            idle.reindexed,
            idle.failed,
            idle.next
        ),
        (5, 0, 0, 0, None)
    );
    let old_pw = r.sealed(ids[0], "password").await.unwrap();

    assert_eq!(r.keyring.rotate().unwrap(), 2);
    // Until it is reindexed, a lookup is still answered, under the previous key version
    // (the rotation window); the full story is in `lookup_survives_a_rotation`.
    assert_eq!(
        r.engine
            .find_by_secret(&access, "account", "token", format!("{TOKEN}0").as_bytes())
            .await
            .unwrap(),
        Some(ids[0])
    );
    // Old values still open under the old key version.
    assert_eq!(
        r.engine
            .open_secret(&access, "account", ids[0], "password")
            .await
            .unwrap()
            .unwrap()
            .expose(),
        format!("{PASSWORD}0")
    );

    // Two at a time.
    let mut after = None;
    let mut seen = Vec::new();
    let (mut rewrapped, mut reindexed) = (0, 0);
    loop {
        let report = r
            .engine
            .rewrap_secrets(&access, "account", RewrapBatch { size: 2, after })
            .await
            .unwrap();
        assert!(report.examined <= 2, "bounded");
        assert_eq!(report.failed, 0);
        seen.push(report.examined);
        rewrapped += report.rewrapped;
        reindexed += report.reindexed;
        match report.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, [2, 2, 1]);
    assert_eq!(rewrapped, 10, "5 passwords and 5 tokens");
    assert_eq!(reindexed, 5, "the 5 token indexes");
    assert_ne!(r.sealed(ids[0], "password").await.unwrap(), old_pw);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            r.engine
                .open_secret(&access, "account", *id, "token")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            format!("{TOKEN}{i}")
        );
        assert_eq!(
            r.engine
                .find_by_secret(
                    &access,
                    "account",
                    "token",
                    format!("{TOKEN}{i}").as_bytes()
                )
                .await
                .unwrap(),
            Some(*id)
        );
    }
    // Now the old key can be retired; every value is still readable, and a second pass is idle.
    r.keyring.retire(1, false).unwrap();
    for id in &ids {
        assert!(
            r.engine
                .open_secret(&access, "account", *id, "password")
                .await
                .unwrap()
                .is_some()
        );
    }
    let again = r
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(100))
        .await
        .unwrap();
    assert_eq!((again.rewrapped, again.reindexed, again.failed), (0, 0, 0));
    // A batch size out of range is refused.
    for size in [0, RewrapBatch::MAX_SIZE + 1] {
        assert_eq!(
            r.engine
                .rewrap_secrets(&access, "account", RewrapBatch::first(size))
                .await
                .unwrap_err()
                .code,
            "secretfield.invalid_batch"
        );
    }
}

async fn a_value_that_cannot_be_rewrapped_is_left_alone_and_reported<
    B: StorageBackend + ChangeFeed,
>(
    r: &Rig<B>,
) {
    let access = r.access();
    let (good, vg) = r.account("good").await;
    let (bad, vb) = r.account("bad").await;
    r.engine
        .set_secret(&access, "account", good, "password", s(PASSWORD), vg)
        .await
        .unwrap();
    r.engine
        .set_secret(&access, "account", bad, "password", s("lost"), vb)
        .await
        .unwrap();
    // Corrupt one row behind the engine's back, then rotate.
    r.poke(
        bad,
        "password__sealed",
        Value::Bytes(b"SPS1 garbage".to_vec()),
    )
    .await;
    r.keyring.rotate().unwrap();
    let report = r
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!((report.rewrapped, report.failed), (1, 1));
    assert_eq!(
        r.sealed(bad, "password").await.unwrap(),
        b"SPS1 garbage".to_vec(),
        "left untouched"
    );
    assert!(
        r.engine
            .open_secret(&access, "account", good, "password")
            .await
            .is_ok()
    );
    let failures: Vec<_> = r
        .engine
        .secret_audit_log()
        .into_iter()
        .filter(|e| e.op == SecretOp::Rewrap && e.id == Some(bad))
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(
        failures[0].error.as_deref(),
        Some("secretfield.rewrap_failed")
    );
}

cases!(
    rotation_rewraps_in_bounded_resumable_batches,
    a_value_that_cannot_be_rewrapped_is_left_alone_and_reported,
);

// ---------------------------------------------------------------------------------------------
// A batch that dies before its commit changes nothing, and the next call finishes the job
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_rewrap_batch_that_crashes_changes_nothing_and_resumes() {
    use crate::flaky::{Fault, FlakyBackend};
    use spot_storage_contract::ErrorKind;

    let keyring = keyring();
    let rig = build(FlakyBackend::new(FakeBackend::new()), keyring.clone(), None);
    rig.engine.prepare().await.unwrap();
    let access = rig.access();
    let mut ids = Vec::new();
    for i in 0..3 {
        let (id, v) = rig.account(&format!("c{i}")).await;
        rig.engine
            .set_secret(
                &access,
                "account",
                id,
                "password",
                s(&format!("{PASSWORD}{i}")),
                v,
            )
            .await
            .unwrap();
        ids.push(id);
    }
    let before: Vec<_> = futures_sealed(&rig, &ids).await;
    keyring.rotate().unwrap();
    // The commit of the batch fails, and keeps failing past the retry budget.
    for _ in 0..4 {
        rig.engine
            .backend()
            .inject(Fault::rolled_back(ErrorKind::Unavailable));
    }
    let err = rig
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Unavailable);
    assert_eq!(
        futures_sealed(&rig, &ids).await,
        before,
        "a failed batch changed nothing"
    );
    // The next call finishes the job.
    let report = rig
        .engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();
    assert_eq!(
        (report.examined, report.rewrapped, report.failed),
        (3, 3, 0)
    );
    assert_ne!(futures_sealed(&rig, &ids).await, before);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            rig.engine
                .open_secret(&access, "account", *id, "password")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            format!("{PASSWORD}{i}")
        );
    }
}
