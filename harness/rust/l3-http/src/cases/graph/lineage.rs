//! Lineage over HTTP: the document chain, a batch traced forward and back, a bill of materials
//! with exact decimal strings, where-used, truncation by `limit` and by depth.

use serde_json::json;

use super::answer::{Answer, Lineage, exact, lineage, lineage_until};
use super::host::{
    FAST, GraphHost, INVOICE_T, ITEM_T, LINEAGE, ORDER_T, PAYMENT_T, RECEIPT_T, Start,
};
use crate::report::CaseResult;
use crate::{ensure, failure};

pub async fn lineage_answers_chain_trace_bom_and_where_used() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        lineage: Some(LINEAGE),
        ..Start::default()
    })
    .await?;
    let result = questions(&host).await;
    host.finish().await;
    result
}

async fn questions(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    let (order, _) = host.make(admin, ORDER_T, &json!({"ref": "O-L"})).await?;
    let (receipt, _) = host
        .make(admin, RECEIPT_T, &json!({"source": order}))
        .await?;
    let (invoice, _) = host
        .make(
            admin,
            INVOICE_T,
            &json!({"receipt": receipt, "number": "L-1", "amount": "5.00"}),
        )
        .await?;
    let (payment, _) = host
        .make(
            admin,
            PAYMENT_T,
            &json!({"invoice": invoice, "amount": "5.00"}),
        )
        .await?;

    // The chain around the order (both ways): all four records.
    let all = Answer::set(&[&order, &receipt, &invoice, &payment]);
    let chain = lineage_until(admin, "chain", ORDER_T, &order, None, |l| l.ids() == all).await?;
    ensure!(
        chain.ids() == all && !chain.truncated && !chain.withheld,
        "the chain is not the four documents: {chain:?}"
    );
    ensure!(
        chain.level_ids(0) == Answer::set(&[&order]),
        "level 0 of a chain is not the start record: {chain:?}"
    );
    // Forward from the receipt (each document links its predecessor): invoice and payment, not
    // the order; back: the order, not the invoice.
    let forward =
        Lineage::from(&lineage(admin, "trace_forward", RECEIPT_T, &receipt, None, None).await?)?;
    ensure!(
        forward.ids().contains(&invoice)
            && forward.ids().contains(&payment)
            && !forward.ids().contains(&order),
        "a forward trace from the receipt is wrong: {forward:?}"
    );
    let back =
        Lineage::from(&lineage(admin, "trace_back", RECEIPT_T, &receipt, None, None).await?)?;
    ensure!(
        back.ids().contains(&order) && !back.ids().contains(&invoice),
        "a backward trace from the receipt is wrong: {back:?}"
    );

    // Truncation: a node limit of 2 cuts the chain short and says so; a depth of 1 does too.
    let limited = Lineage::from(&lineage(admin, "chain", ORDER_T, &order, None, Some(2)).await?)?;
    ensure!(
        limited.truncated && limited.reason.as_deref() == Some("nodes"),
        "a limit of 2 did not truncate with the reason `nodes`: {limited:?}"
    );
    ensure!(
        limited.ids().len() <= 2 && limited.ids().is_subset(&all),
        "the truncated answer holds more than the limit or a foreign node: {limited:?}"
    );
    let shallow = Lineage::from(&lineage(admin, "chain", ORDER_T, &order, Some(1), None).await?)?;
    ensure!(
        shallow.truncated && !shallow.ids().contains(&payment),
        "a depth of 1 did not truncate the chain: {shallow:?}"
    );

    bill_of_materials(host).await
}

/// A tree of items: a root, one sub-assembly (quantity 1) and two leaves under it whose
/// quantities a float adds wrongly; a second tree whose total a float cannot hold.
async fn bill_of_materials(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    let root = host
        .make(admin, ITEM_T, &json!({"title": "Root", "qty": "1"}))
        .await?
        .0;
    let sub = host
        .make(
            admin,
            ITEM_T,
            &json!({"title": "Sub", "parent_item": root, "qty": "1"}),
        )
        .await?
        .0;
    let leaf_a = host
        .make(
            admin,
            ITEM_T,
            &json!({"title": "A", "parent_item": sub, "qty": "0.10"}),
        )
        .await?
        .0;
    let leaf_b = host
        .make(
            admin,
            ITEM_T,
            &json!({"title": "B", "parent_item": sub, "qty": "0.20"}),
        )
        .await?
        .0;
    let bom = lineage_until(admin, "bom", ITEM_T, &root, None, |l| {
        l.level_ids(2) == Answer::set(&[&leaf_a, &leaf_b])
    })
    .await?;
    ensure!(
        bom.level_ids(1) == Answer::set(&[&sub])
            && bom.level_ids(2) == Answer::set(&[&leaf_a, &leaf_b]),
        "the exploded tree has the wrong levels: {bom:?}"
    );
    // Exact: 0.10 + 0.20 is 0.3 (a float gives 0.30000000000000004); every quantity is a string.
    let total = bom
        .total(2)
        .ok_or_else(|| failure!("level 2 carries no quantity_total: {bom:?}"))?;
    ensure!(
        exact(&total, "the level 2 total")? == "0.3",
        "0.10 + 0.20 is not 0.3: {total}"
    );
    for node in bom.nodes(2) {
        let quantity = node
            .get("quantity")
            .ok_or_else(|| failure!("a bill of materials node has no quantity: {node}"))?;
        let text = exact(quantity, "a node quantity")?;
        let id = node.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let want = if id == leaf_a { "0.1" } else { "0.2" };
        ensure!(text == want, "the quantity of a leaf is {text}, not {want}");
    }

    // Beyond what 2^53 holds: 99999999999999999.99 + 0.01 = 100000000000000000.
    let big = host
        .make(admin, ITEM_T, &json!({"title": "Big", "qty": "1"}))
        .await?
        .0;
    host.make(
        admin,
        ITEM_T,
        &json!({"title": "X", "parent_item": big, "qty": "99999999999999999.99"}),
    )
    .await?;
    host.make(
        admin,
        ITEM_T,
        &json!({"title": "Y", "parent_item": big, "qty": "0.01"}),
    )
    .await?;
    let huge = lineage_until(admin, "bom", ITEM_T, &big, Some(1), |l| {
        l.nodes(1).len() == 2
    })
    .await?;
    let total = huge
        .total(1)
        .ok_or_else(|| failure!("level 1 carries no quantity_total: {huge:?}"))?;
    ensure!(
        exact(&total, "the level 1 total")? == "100000000000000000",
        "99999999999999999.99 + 0.01 lost precision: {total}"
    );

    // Where used: a leaf goes into its sub-assembly and the root, not into its sibling.
    let used = lineage_until(admin, "where_used", ITEM_T, &leaf_a, None, |l| {
        l.ids().contains(&root)
    })
    .await?;
    ensure!(
        used.ids().contains(&sub) && used.ids().contains(&root) && !used.ids().contains(&leaf_b),
        "where-used of a leaf is wrong: {used:?}"
    );
    Ok(())
}
