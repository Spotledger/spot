//! The contract every executor must meet: the same scenarios give equal answers on two
//! executors. The in-memory index (WP-GRA-02) runs this against the storage executor, so its
//! tests need no design of their own.

use std::fmt::Debug;
use std::sync::Arc;

use spot_permissions::{PermissionEngine, Principal};

use crate::exec::{GraphExecutor, Invocation};
use crate::query::GraphQuery;

/// One question with the principal and the permission rules it is asked under.
#[derive(Clone)]
pub struct Scenario {
    /// A name for reports.
    pub name: String,
    /// The validated query.
    pub query: GraphQuery,
    /// Who asks.
    pub principal: Principal,
    /// The rules that answer.
    pub engine: Arc<PermissionEngine>,
}

/// The first scenario on which the two executors disagree, with both outcomes.
#[derive(Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The scenario's name.
    pub scenario: String,
    /// The first executor's outcome, as text.
    pub left: String,
    /// The second executor's outcome, as text.
    pub right: String,
}

/// Runs every scenario on both executors and compares the outcomes: equal results, or the same
/// error code, pass.
///
/// # Errors
///
/// The first [`Divergence`].
pub async fn same_answers<A: GraphExecutor, B: GraphExecutor>(
    left: &mut A,
    right: &mut B,
    scenarios: &[Scenario],
) -> Result<(), Divergence> {
    for scenario in scenarios {
        let who = Invocation {
            principal: &scenario.principal,
            engine: &scenario.engine,
        };
        let a = left.execute(&scenario.query, &who).await;
        let b = right.execute(&scenario.query, &who).await;
        let same = match (&a, &b) {
            (Ok(x), Ok(y)) => x == y,
            (Err(x), Err(y)) => x.code() == y.code(),
            _ => false,
        };
        if !same {
            return Err(Divergence {
                scenario: scenario.name.clone(),
                left: outcome(&a),
                right: outcome(&b),
            });
        }
    }
    Ok(())
}

fn outcome<T: Debug, E: Debug>(result: &Result<T, E>) -> String {
    match result {
        Ok(value) => format!("{value:?}"),
        Err(error) => format!("error {error:?}"),
    }
}
