//! Reading one bounded page at a time, and the keyset predicate that makes paging exact.
//!
//! A page is one short read outside any transaction: [`PageReader`] is the narrow seam the
//! export uses, so no transaction is held across chunks. [`BackendPageReader`] adapts any
//! [`StorageBackend`].
//!
//! Paging is **keyset**, never `OFFSET`: [`after_predicate`] builds the predicate that says
//! "strictly after the last delivered row" in the plan's sort order, so a row inserted behind
//! the cursor is not delivered and the scan never restarts from the beginning.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_storage_contract::{
    Catalog, CmpOp, Expr, Name, Nulls, OrderDir, Query, ReadSource, Rows, StorageBackend,
    StorageResult, Value,
};

use crate::spec::OrderKey;

/// A boxed, `Send` future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs one bounded page query outside a transaction.
pub trait PageReader: Send + Sync {
    /// Runs `query` and returns its rows.
    fn read<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>>;
}

/// A [`PageReader`] over any storage backend and a catalog snapshot.
pub struct BackendPageReader<B: StorageBackend> {
    backend: B,
    catalog: Arc<Catalog>,
}

impl<B: StorageBackend> BackendPageReader<B> {
    /// Reads pages from `backend` against `catalog`.
    pub fn new(backend: B, catalog: Arc<Catalog>) -> Self {
        BackendPageReader { backend, catalog }
    }

    /// The backend, for a caller that also writes through it.
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: StorageBackend> PageReader for BackendPageReader<B> {
    fn read<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async move {
            self.backend
                .read(self.catalog.clone(), ReadSource::Primary, query)
                .await
        })
    }
}

/// The predicate "strictly after the last delivered row" in `alias`'s sort order, with the
/// record id as the implicit final tie-break.
///
/// `keys` holds one value per order key in order, followed by the last delivered record id.
/// Null placement is explicit: a null key with nulls-first means every remaining row is after
/// it, and a null key with nulls-last means only the remaining nulls are.
#[must_use]
pub fn after_predicate(alias: &Name, order: &[OrderKey], keys: &[Value]) -> Expr {
    let last_id = keys.get(order.len());
    after_impl(alias, order, keys, last_id)
}

/// The predicate "strictly after the last delivered row" for a tuple that **is** the order keys
/// (no implicit id): used by a child part, whose key is `(parent, idx)`.
#[must_use]
pub fn after_predicate_exact(alias: &Name, order: &[OrderKey], keys: &[Value]) -> Expr {
    after_impl(alias, order, keys, None)
}

fn after_impl(alias: &Name, order: &[OrderKey], keys: &[Value], last_id: Option<&Value>) -> Expr {
    let mut result = match last_id {
        Some(id) => Expr::Cmp(
            Box::new(Expr::field(alias, &name("id"))),
            CmpOp::Gt,
            Box::new(Expr::Value(id.clone())),
        ),
        // The exact-tuple variant: the innermost key must be strict, so its equality branch is
        // dropped (an equality AND false is false, leaving the strict comparison alone).
        None => false_expr(),
    };
    // Fold the order keys from last to first: `rest` accumulates the tuple comparison.
    for (i, key) in order.iter().enumerate().rev() {
        let value = keys.get(i).unwrap_or(&Value::Null);
        let field = Expr::field(alias, &name(&key.field));
        let (after, equal) = match value {
            Value::Null => {
                let is_null = Expr::IsNull(Box::new(field.clone()));
                let after = if key.nulls == Nulls::First {
                    None
                } else {
                    Some(is_null.clone())
                };
                (after, is_null)
            }
            other => (
                Some(compare_after(field.clone(), key.dir, key.nulls, other)),
                Expr::Cmp(
                    Box::new(field),
                    CmpOp::Eq,
                    Box::new(Expr::Value(other.clone())),
                ),
            ),
        };
        let rest = Expr::And(vec![equal, result]);
        result = match after {
            None => true_expr(),
            Some(after) => Expr::Or(vec![after, rest]),
        };
    }
    result
}

/// `field <dir> value`, plus `field IS NULL` when nulls sort last and `value` is non-null.
fn compare_after(field: Expr, dir: OrderDir, nulls: Nulls, value: &Value) -> Expr {
    let op = match dir {
        OrderDir::Asc => CmpOp::Gt,
        OrderDir::Desc => CmpOp::Lt,
    };
    let cmp = Expr::Cmp(
        Box::new(field.clone()),
        op,
        Box::new(Expr::Value(value.clone())),
    );
    match nulls {
        Nulls::Last => Expr::Or(vec![cmp, Expr::IsNull(Box::new(field))]),
        Nulls::First => cmp,
    }
}

/// A predicate that is always true (`NOT FALSE`); the IR has no boolean literal.
fn true_expr() -> Expr {
    Expr::Not(Box::new(Expr::Or(Vec::new())))
}

/// A predicate that is always false; the IR has no boolean literal.
fn false_expr() -> Expr {
    Expr::Or(Vec::new())
}

/// A name from text that the plan already validated; the fallback is a valid name.
fn name(s: &str) -> Name {
    match Name::new(s) {
        Ok(n) => n,
        Err(_) => match Name::new("x") {
            Ok(n) => n,
            Err(_) => unreachable!("`x` is a valid storage name"),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn asc_nulls_last(field: &str) -> OrderKey {
        OrderKey {
            field: field.to_owned(),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }
    }

    #[test]
    fn a_non_null_key_with_nulls_last_also_reaches_the_nulls() {
        let keys = vec![
            Value::Int(5),
            Value::Id(spot_storage_contract::RecordId::new_v7()),
        ];
        let e = after_predicate(&name("d"), &[asc_nulls_last("k")], &keys);
        let text = format!("{e:?}");
        assert!(text.contains("IsNull"), "{text}");
        assert!(text.contains("Gt"), "{text}");
    }

    #[test]
    fn a_null_key_with_nulls_last_only_reaches_the_remaining_nulls() {
        let keys = vec![
            Value::Null,
            Value::Id(spot_storage_contract::RecordId::new_v7()),
        ];
        let e = after_predicate(&name("d"), &[asc_nulls_last("k")], &keys);
        let text = format!("{e:?}");
        // The equality branch is an IsNull, never a comparison with a null literal.
        assert!(!text.contains("Value(Null"), "{text}");
        assert!(text.contains("IsNull"), "{text}");
    }

    #[test]
    fn the_exact_variant_has_no_implicit_id_tiebreak() {
        let keys = vec![
            Value::Id(spot_storage_contract::RecordId::new_v7()),
            Value::Int(2),
        ];
        let e = after_predicate_exact(
            &name("c"),
            &[
                OrderKey {
                    field: "parent".to_owned(),
                    dir: OrderDir::Asc,
                    nulls: Nulls::Last,
                },
                OrderKey {
                    field: "idx".to_owned(),
                    dir: OrderDir::Asc,
                    nulls: Nulls::Last,
                },
            ],
            &keys,
        );
        let text = format!("{e:?}");
        // The exact variant never compares `id` (a child has no such key).
        assert!(!text.contains("field: \"id\""), "{text}");
    }
}
