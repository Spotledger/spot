//! Edges follow document writes (create, change a link, delete, a rolled-back write), and the
//! index survives a restart and a gap in its feed and equals the database afterwards.

use std::time::Duration;

use serde_json::json;

use super::answer::{Answer, answer, query, until};
use super::host::{
    FAST, GraphHost, INVOICE_EDGE, INVOICE_T, ORDER_T, PAYMENT_EDGE, PAYMENT_T, RECEIPT_EDGE,
    RECEIPT_T, STORAGE_ONLY, Start,
};
use crate::report::CaseResult;
use crate::{ensure, failure};

pub async fn edges_follow_document_writes() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        ..Start::default()
    })
    .await?;
    let result = writes(&host).await;
    host.finish().await;
    result
}

async fn writes(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    let (order, _) = host.make(admin, ORDER_T, &json!({"ref": "O-1"})).await?;
    let (r1, _) = host
        .make(admin, RECEIPT_T, &json!({"source": order}))
        .await?;
    let (invoice, invoice_version) = host
        .make(
            admin,
            INVOICE_T,
            &json!({"receipt": r1, "number": "INV-1", "amount": "10.00"}),
        )
        .await?;
    let (payment, payment_version) = host
        .make(
            admin,
            PAYMENT_T,
            &json!({"invoice": invoice, "amount": "10.00"}),
        )
        .await?;

    // Create: the chain is one walk from the payment to the order.
    let all = Answer::set(&[&order, &r1, &invoice, &payment]);
    let found = until(
        admin,
        &query(PAYMENT_T, &payment, "out", 3, &[]),
        "the whole chain",
        |a| a.nodes == all,
    )
    .await?;
    ensure!(
        found.edges.len() == 3 && !found.withheld && !found.truncated,
        "the chain is not three plain edges: {found:?}"
    );
    ensure!(
        found
            .edges
            .contains(&(payment.clone(), invoice.clone(), PAYMENT_EDGE.to_owned()))
            && found
                .edges
                .contains(&(invoice.clone(), r1.clone(), INVOICE_EDGE.to_owned()))
            && found
                .edges
                .contains(&(r1.clone(), order.clone(), RECEIPT_EDGE.to_owned())),
        "an edge of the chain has the wrong ends or type: {:?}",
        found.edges
    );
    // The other direction reaches the same four records.
    let back = until(
        admin,
        &query(ORDER_T, &order, "in", 3, &[]),
        "the whole chain from the order",
        |a| a.nodes == all,
    )
    .await?;
    ensure!(
        back.edges == found.edges,
        "in and out disagree on the edges"
    );

    // Change a link: the invoice moves to a second receipt of the same order.
    let (r2, _) = host
        .make(admin, RECEIPT_T, &json!({"source": order}))
        .await?;
    let moved = admin
        .update(
            INVOICE_T,
            &invoice,
            invoice_version,
            &json!({"receipt": r2}),
        )
        .await?;
    moved.expect(200)?;
    let after = Answer::set(&[&order, &r2, &invoice, &payment]);
    until(
        admin,
        &query(PAYMENT_T, &payment, "out", 3, &[]),
        "the chain through the second receipt",
        |a| a.nodes == after,
    )
    .await?;
    let first = until(
        admin,
        &query(RECEIPT_T, &r1, "in", 1, &[]),
        "the first receipt without its invoice",
        |a| a.nodes == Answer::set(&[&r1]),
    )
    .await?;
    ensure!(
        first.edges.is_empty(),
        "an edge of the moved link is left: {first:?}"
    );

    // A rolled-back write leaves no edge: a second invoice with the same unique number is
    // refused whole, so neither the document nor its edge exists.
    let before = host
        .count(
            host.site(),
            "SELECT count(*) FROM spot_graph_edge WHERE edge_type LIKE 'chain_invoice%'",
        )
        .await?;
    let refused = admin
        .create(
            INVOICE_T,
            &json!({"receipt": r2, "number": "INV-1", "amount": "99.00"}),
        )
        .await?;
    ensure!(
        (400..500).contains(&refused.status),
        "the duplicate invoice number was not refused: {}",
        refused.text()
    );
    let kept = host
        .count(
            host.site(),
            "SELECT count(*) FROM spot_graph_edge WHERE edge_type LIKE 'chain_invoice%'",
        )
        .await?;
    ensure!(
        kept == before,
        "a refused write left {} edge rows behind (had {before}, now {kept})",
        kept - before
    );
    let second = answer(admin, &query(RECEIPT_T, &r2, "in", 1, &[])).await?;
    ensure!(
        second.nodes == Answer::set(&[&r2, &invoice]),
        "the refused invoice shows in the graph: {second:?}"
    );

    // Delete: the payment's edge goes with the payment.
    let deleted = admin.delete(PAYMENT_T, &payment, payment_version).await?;
    ensure!(
        deleted.status < 300,
        "the payment was not deleted: {}",
        deleted.text()
    );
    let without = until(
        admin,
        &query(INVOICE_T, &invoice, "in", 1, &[]),
        "the invoice without its payment",
        |a| !a.nodes.contains(&payment),
    )
    .await?;
    ensure!(
        without.nodes == Answer::set(&[&invoice]) && without.edges.is_empty(),
        "a deleted payment is still reachable: {without:?}"
    );
    Ok(())
}

pub async fn the_index_survives_a_restart_and_a_gap() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        ..Start::default()
    })
    .await?;
    let result = restart_and_gap(&mut host).await;
    host.finish().await;
    result
}

async fn restart_and_gap(host: &mut GraphHost) -> CaseResult {
    let (order, _) = host
        .make(&host.admin, ORDER_T, &json!({"ref": "O-GAP"}))
        .await?;
    let mut receipts = Vec::new();
    for _ in 0..3 {
        let (id, _) = host
            .make(&host.admin, RECEIPT_T, &json!({"source": order}))
            .await?;
        receipts.push(id);
    }
    let body = query(ORDER_T, &order, "in", 2, &[]);
    let everything = Answer::set(&[&order, &receipts[0], &receipts[1], &receipts[2]]);
    let before = until(&host.admin, &body, "the order with its receipts", |a| {
        a.nodes == everything
    })
    .await?;
    // The first answers came from the database; let the index build and take over.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Restart the server mid-feed: the same answer comes back, from a fresh index.
    host.restart(FAST).await?;
    let restarted = until(&host.admin, &body, "the same answer after a restart", |a| {
        a.nodes == everything
    })
    .await?;
    ensure!(
        restarted == before,
        "a restart changed the answer: {before:?} then {restarted:?}"
    );

    // A gap: an edge disappears from the table with no event behind it, as when the edge consumer
    // dies and misses a change. Only the drift check can see it; the index then rebuilds.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let removed = host
        .execute(
            host.site(),
            &format!(
                "DELETE FROM spot_graph_edge WHERE from_id::text = '{}'",
                digits_and_dashes(&receipts[0])?
            ),
        )
        .await?;
    ensure!(
        removed >= 1,
        "the edge table held no edge of the first receipt"
    );
    // The documents are authoritative: the verifier finds the edge the table lost and repairs it,
    // and the index follows the table back to the whole chain.
    let gapped = until(
        &host.admin,
        &body,
        "the answer with the lost edge repaired",
        |a| a.nodes == everything,
    )
    .await?;

    // Equality: the hot index and the database alone give the same answer.
    host.restart(STORAGE_ONLY).await?;
    let database = answer(&host.admin, &body).await?;
    ensure!(
        database == gapped,
        "the index and the database disagree after a gap: {gapped:?} against {database:?}"
    );
    host.restart(FAST).await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let rebuilt = answer(&host.admin, &body).await?;
    ensure!(
        rebuilt == database,
        "the rebuilt index differs from the database: {rebuilt:?} against {database:?}"
    );
    Ok(())
}

/// A record id as it may be written into a statement: hexadecimal digits and dashes only.
pub fn digits_and_dashes(id: &str) -> Result<&str, crate::report::Failure> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        Ok(id)
    } else {
        Err(failure!("`{id}` is not a record id"))
    }
}
