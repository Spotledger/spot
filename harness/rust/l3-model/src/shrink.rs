//! Shrinking a failing sequence to a minimal one.

use std::future::Future;
use std::pin::Pin;

use crate::model::Call;

/// A boxed check: `true` when the sequence still diverges.
pub type Check<'a> =
    Box<dyn FnMut(Vec<Call>) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> + Send + 'a>;

/// Reduces `calls` while `still_fails` keeps saying it fails: drop calls from the end, then drop
/// single calls from the front, then repeat until nothing more can go. Slots make every
/// subsequence meaningful (a call on a slot that was never created is skipped).
pub async fn shrink(calls: Vec<Call>, mut still_fails: Check<'_>) -> Vec<Call> {
    let mut best = calls;
    // From the end: the shortest prefix that still fails.
    let mut len = 1;
    while len < best.len() {
        let prefix = best[..len].to_vec();
        if still_fails(prefix.clone()).await {
            best = prefix;
            break;
        }
        len += 1;
    }
    // Single calls: drop one at a time, keep the drop when it still fails, until a full pass
    // changes nothing.
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < best.len() {
            let mut candidate = best.clone();
            candidate.remove(i);
            if !candidate.is_empty() && still_fails(candidate.clone()).await {
                best = candidate;
                changed = true;
            } else {
                i += 1;
            }
        }
        if !changed {
            break;
        }
    }
    best
}

/// The sequence as the body of a Rust test, to paste and replay.
pub fn to_rust(seed: u64, calls: &[Call]) -> String {
    let mut out = format!(
        "// seed {seed}\nuse spot_l3_model::model::{{Call::*, Ver::*, Who::*}};\nlet calls = vec![\n"
    );
    for c in calls {
        out.push_str(&format!("    {c:?},\n"));
    }
    out.push_str("];\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Ver, Who};

    #[tokio::test]
    async fn a_failing_pair_among_noise_shrinks_to_the_pair() {
        let noise = Call::CreateTask { slot: 2 };
        let create = Call::CreateTe { slot: 0, task: 0 };
        let cancel = Call::Cancel {
            te: 0,
            ver: Ver::Current,
            who: Who::Manager,
        };
        let calls = vec![noise, noise, create, noise, cancel, noise];
        let check: Check<'_> = Box::new(move |c: Vec<Call>| {
            Box::pin(async move { c.contains(&create) && c.contains(&cancel) })
        });
        let small = shrink(calls, check).await;
        assert_eq!(small, vec![create, cancel]);
    }

    #[test]
    fn the_sequence_prints_as_a_rust_body() {
        let text = to_rust(5, &[Call::CreateTask { slot: 0 }]);
        assert!(text.contains("seed 5") && text.contains("CreateTask { slot: 0 }"));
    }
}
