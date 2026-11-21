//! Permissions over HTTP: a record the caller may not read is removed with every path through it,
//! the answer says only `withheld`, and an unreadable start is the answer of an unknown one.

use std::time::Duration;

use serde_json::json;

use super::answer::{Answer, Lineage, answer, ask, error_of, lineage, query};
use super::host::{
    FAST, GraphHost, INVOICE_T, LINEAGE, ORDER_T, PAYMENT_T, RECEIPT_T, Start, UNKNOWN_ID,
};
use crate::ensure;
use crate::fixture::{OBSERVER, VIEWER};
use crate::report::CaseResult;

pub async fn queries_and_lineage_respect_permissions() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        lineage: Some(LINEAGE),
        ..Start::default()
    })
    .await?;
    let result = permissions(&host).await;
    host.finish().await;
    result
}

async fn permissions(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    // The chain O <- R <- I <- P, and a lone order O3 with no receipt: the graph without R.
    let (order, _) = host.make(admin, ORDER_T, &json!({"ref": "O-P"})).await?;
    let (receipt, _) = host
        .make(admin, RECEIPT_T, &json!({"source": order}))
        .await?;
    let (invoice, _) = host
        .make(
            admin,
            INVOICE_T,
            &json!({"receipt": receipt, "number": "P-1", "amount": "1.00"}),
        )
        .await?;
    let (payment, _) = host
        .make(
            admin,
            PAYMENT_T,
            &json!({"invoice": invoice, "amount": "1.00"}),
        )
        .await?;
    let (lone, _) = host.make(admin, ORDER_T, &json!({"ref": "O-3"})).await?;

    let viewer = host.sign_in(VIEWER).await?;
    let observer = host.sign_in(OBSERVER).await?;
    // The grants hold: the Viewer reads the order and not the receipt.
    viewer.document(ORDER_T, &order).await?.expect(200)?;
    let denied = viewer.document(RECEIPT_T, &receipt).await?;
    ensure!(
        denied.status == 403 || denied.status == 404,
        "the Viewer reads a receipt: {}",
        denied.text()
    );

    // The Administrator sees the whole chain.
    let body = query(ORDER_T, &order, "in", 3, &[]);
    let everything = Answer::set(&[&order, &receipt, &invoice, &payment]);
    let admin_view =
        super::answer::until(admin, &body, "the whole chain", |a| a.nodes == everything).await?;
    ensure!(
        !admin_view.withheld,
        "the Administrator is told something is withheld"
    );

    // The Viewer cannot read the receipt: it and its descendants (invoice, payment) are gone and
    // the answer equals the one for an order with no receipts, apart from the flag. Asked twice:
    // at once (the database) and after the index had time to become hot.
    let lone_body = query(ORDER_T, &lone, "in", 3, &[]);
    for round in 0..2 {
        if round == 1 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let hidden = answer(&viewer, &body).await?;
        let control = answer(&viewer, &lone_body).await?;
        ensure!(
            hidden.nodes == Answer::set(&[&order]) && hidden.edges.is_empty(),
            "the Viewer's answer holds the receipt or what hangs off it: {hidden:?}"
        );
        ensure!(
            hidden.withheld,
            "the Viewer is not told something was withheld: {hidden:?}"
        );
        ensure!(
            !control.withheld,
            "an order with no receipts is reported as withheld: {control:?}"
        );
        ensure!(
            (
                hidden.nodes.len(),
                hidden.edges.len(),
                hidden.truncated,
                &hidden.reason
            ) == (
                control.nodes.len(),
                control.edges.len(),
                control.truncated,
                &control.reason
            ),
            "the answer with a hidden record differs from the graph without it: {hidden:?} against {control:?}"
        );
        // Reaching the hidden records from the other end removes them as well.
        let from_payment = answer(&viewer, &query(PAYMENT_T, &payment, "out", 3, &[])).await?;
        ensure!(
            !from_payment.nodes.contains(&receipt) && !from_payment.nodes.contains(&order),
            "a walk from the payment passes through the hidden receipt: {from_payment:?}"
        );
        ensure!(
            from_payment.withheld,
            "a walk through a hidden record does not say withheld: {from_payment:?}"
        );
    }

    // Lineage applies the same filter.
    let chain = Lineage::from(&lineage(&viewer, "chain", ORDER_T, &order, None, None).await?)?;
    ensure!(
        chain.ids() == Answer::set(&[&order]) && chain.withheld,
        "the Viewer's lineage holds the hidden chain or is silent about it: {chain:?}"
    );

    // An unreadable start is the answer of an unknown one: the same status and the same error,
    // for a user who reads other things (the Viewer, the receipt) and for one who reads nothing
    // (the Observer, an order).
    let unknown = ask(&viewer, &query(ORDER_T, UNKNOWN_ID, "in", 1, &[])).await?;
    unknown.expect(404)?.expect_error("graph.not_found")?;
    for (client, doctype, id, who) in [
        (
            &viewer,
            RECEIPT_T,
            &receipt,
            "a Viewer asking for a receipt",
        ),
        (
            &observer,
            ORDER_T,
            &order,
            "an Observer asking for an order",
        ),
    ] {
        let reply = ask(client, &query(doctype, id, "in", 1, &[])).await?;
        reply.expect(404)?.expect_error("graph.not_found")?;
        ensure!(
            error_of(&reply) == error_of(&unknown),
            "{who} gets an answer that differs from an unknown record's: {} against {}",
            reply.text(),
            unknown.text()
        );
        for kind in ["chain", "trace_forward", "bom", "where_used"] {
            let got = lineage(client, kind, doctype, id, None, None).await?;
            let none = lineage(client, kind, doctype, UNKNOWN_ID, None, None).await?;
            // A bill of materials is configured for the item DocType only, so `bom` and
            // `where_used` on a document of the chain are a configuration refusal that is the same
            // for a real record and an unknown one; the other kinds are `graph.not_found`.
            let want = if matches!(kind, "bom" | "where_used") {
                "lineage.kind_unknown"
            } else {
                "graph.not_found"
            };
            ensure!(
                got.status == none.status
                    && error_of(&got) == error_of(&none)
                    && got.error_code().as_deref() == Some(want),
                "{who} (lineage `{kind}`) is told more than for an unknown record: {} against {}",
                got.text(),
                none.text()
            );
        }
    }

    // No credential, and a forged one: 401, the same for both.
    let anonymous = ask(&host.anonymous(host.site()), &body).await?;
    anonymous.expect(401)?;
    let forged = ask(
        &host
            .anonymous(host.site())
            .with_token("not-a-session-token"),
        &body,
    )
    .await?;
    forged.expect(401)?;
    ensure!(
        forged.error_code().is_some(),
        "a forged token is not refused like a missing one: {}",
        forged.text()
    );
    Ok(())
}
