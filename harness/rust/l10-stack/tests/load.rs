//! Load liveness (WP-TST-11.T3): eight writers create documents through node A at full speed
//! while thirty custom fields are added through the service. No writer sees an error caused by a
//! schema change, each writer sees the newest field within one call after its change, and the
//! version rises by exactly one per change.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use spot_l10_stack::StackHost;

const WRITERS: usize = 8;
const CHANGES: usize = 30;

#[test]
fn eight_writers_see_thirty_field_additions_with_no_failure() {
    let Some(host) = StackHost::boot() else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run the L10 stack");
        return;
    };
    let stop = AtomicBool::new(false);
    // The number of fields whose addition has returned: a writer reading it afterwards must be
    // able to use that field in its very next call.
    let published = AtomicUsize::new(0);
    let writes = AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::<String>::new());
    let mut timings = Vec::new();

    std::thread::scope(|scope| {
        for w in 0..WRITERS {
            let writer = host.writer();
            let (stop, published, writes, failures) = (&stop, &published, &writes, &failures);
            scope.spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::SeqCst) {
                    let latest = published.load(Ordering::SeqCst);
                    let field = (latest > 0).then(|| format!("custom_{latest}"));
                    let code = format!("w{w}-{n}");
                    n += 1;
                    match writer.write_through(&code, field.as_deref()) {
                        Ok(()) => {
                            writes.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => failures
                            .lock()
                            .unwrap()
                            .push(format!("writer {w}, field {field:?}: {e}")),
                    }
                }
            });
        }
        let mut version = host.version();
        for i in 1..=CHANGES {
            let started = Instant::now();
            let applied = host.add_field(&format!("custom_{i}")).unwrap();
            timings.push(started.elapsed());
            assert_eq!(
                applied,
                version + 1,
                "the version rises by exactly one per change"
            );
            version = applied;
            published.store(i, Ordering::SeqCst);
            // Let the writers run a few calls on the new field before the next change.
            let seen = writes.load(Ordering::SeqCst);
            while writes.load(Ordering::SeqCst) < seen + WRITERS {
                std::thread::yield_now();
            }
        }
        stop.store(true, Ordering::SeqCst);
    });

    let failures = failures.into_inner().unwrap();
    let total = writes.load(Ordering::SeqCst);
    let slowest = timings.iter().max().copied().unwrap_or_default();
    println!("{total} writes, {CHANGES} changes, slowest apply {slowest:?}");
    assert!(
        failures.is_empty(),
        "{} failures, first: {:?}",
        failures.len(),
        failures.first()
    );
    assert!(
        total >= WRITERS * CHANGES,
        "the writers made progress: {total}"
    );
}
