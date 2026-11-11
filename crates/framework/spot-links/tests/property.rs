//! Properties: link validation never accepts a value whose target the store cannot find.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Harness, name, row, text};
use spot_app_api::prelude::{RecordId, Value};

/// A small deterministic PRNG (xorshift64*), so the property is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let index =
            usize::try_from(self.next() % u64::try_from(items.len()).unwrap_or(1)).unwrap_or(0);
        &items[index]
    }
}

fn path_ok(issues: &[spot_links::FieldIssue], path: &str) -> bool {
    !issues.iter().any(|issue| issue.path.to_string() == path)
}

#[tokio::test]
async fn validation_never_accepts_a_value_the_store_cannot_find() {
    let h = Harness::new().await;
    let mut pool = Vec::new();
    for i in 0..4 {
        pool.push(
            h.create("customer", &[("name", Value::Text(format!("Cust-{i}")))])
                .await
                .id(),
        );
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let types = ["customer", "widget", "invoice_line", "customer"];

    for _ in 0..250 {
        let fixed = if rng.next().is_multiple_of(2) {
            *rng.pick(&pool)
        } else {
            RecordId::new_v7()
        };
        let line = if rng.next().is_multiple_of(2) {
            *rng.pick(&pool)
        } else {
            RecordId::new_v7()
        };

        // A fixed link on the document and one on a child row.
        let mut invoice = spot_app_api::Document::new(name("invoice"), RecordId::new_v7());
        invoice.set(name("number"), text("INV"));
        invoice.set(name("customer"), Value::Id(fixed));
        invoice.rows_mut(name("lines")).push(row(&[
            ("item", text("W")),
            ("billing_customer", Value::Id(line)),
        ]));
        let issues = h.validate("invoice", &invoice).await.expect("runs");
        if path_ok(&issues, "customer") {
            assert!(
                h.stored("customer", fixed).await.is_some(),
                "accepted `customer` {fixed:?} but the store cannot find it"
            );
        }
        if path_ok(&issues, "lines[1].billing_customer") {
            assert!(
                h.stored("customer", line).await.is_some(),
                "accepted `lines[1].billing_customer` {line:?} but the store cannot find it"
            );
        }

        // A dynamic link with a random type and target.
        let type_name = rng.pick(&types).to_owned();
        let mut note = spot_app_api::Document::new(name("note"), RecordId::new_v7());
        note.set(name("about_type"), text(type_name));
        note.set(name("about"), Value::Id(fixed));
        let issues = h.validate("note", &note).await.expect("runs");
        if path_ok(&issues, "about") {
            assert_eq!(type_name, "customer", "only a real top-level type may pass");
            assert!(
                h.stored("customer", fixed).await.is_some(),
                "accepted a dynamic link the store cannot find"
            );
        }
    }
}
