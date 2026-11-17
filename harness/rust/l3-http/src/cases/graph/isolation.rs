//! Two sites of one server share no edge: by query, by lineage, by list, by a session of the
//! other site, before and after a restart (the indexes are rebuilt per site).

use std::time::Duration;

use serde_json::json;

use super::answer::{Answer, ask, error_of, lineage, lineage_until, query, until};
use super::host::{FAST, GraphHost, LINEAGE, ORDER_T, RECEIPT_T, Start, UNKNOWN_ID};
use crate::client::Client;
use crate::report::CaseResult;
use crate::{ensure, failure};

struct Ids {
    order_a: String,
    receipt_a: String,
    order_b: String,
    receipt_b: String,
}

pub async fn two_sites_share_no_edge() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        lineage: Some(LINEAGE),
        second_site: true,
        ..Start::default()
    })
    .await?;
    let result = run(&mut host).await;
    host.finish().await;
    result
}

async fn run(host: &mut GraphHost) -> CaseResult {
    let (site_b, admin_b) = host
        .second
        .clone()
        .ok_or_else(|| failure!("the host made no second site"))?;
    let (order_a, _) = host
        .make(&host.admin, ORDER_T, &json!({"ref": "A-1"}))
        .await?;
    let (receipt_a, _) = host
        .make(&host.admin, RECEIPT_T, &json!({"source": order_a}))
        .await?;
    let (order_b, _) = host.make(&admin_b, ORDER_T, &json!({"ref": "B-1"})).await?;
    let (receipt_b, _) = host
        .make(&admin_b, RECEIPT_T, &json!({"source": order_b}))
        .await?;
    let ids = Ids {
        order_a,
        receipt_a,
        order_b,
        receipt_b,
    };
    // Before a restart the first answers come from the database; after one, from each site's
    // own freshly built index.
    let admin_a = host.admin.clone();
    separated(host, &admin_a, &admin_b, &site_b, &ids).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    host.restart(FAST).await?;
    let admin_b = host
        .second
        .as_ref()
        .map(|(_, client)| client.clone())
        .ok_or_else(|| failure!("the second site is gone after a restart"))?;
    let admin_a = host.admin.clone();
    tokio::time::sleep(Duration::from_secs(3)).await;
    separated(host, &admin_a, &admin_b, &site_b, &ids).await
}

async fn separated(
    host: &GraphHost,
    a: &Client,
    b: &Client,
    site_b: &str,
    ids: &Ids,
) -> CaseResult {
    let (want_a, want_b) = (
        Answer::set(&[&ids.order_a, &ids.receipt_a]),
        Answer::set(&[&ids.order_b, &ids.receipt_b]),
    );
    let found_a = until(a, &query(ORDER_T, &ids.order_a, "in", 2, &[]), "A's order with its receipt", |x| x.nodes == want_a).await?;
    ensure!(
        found_a.nodes == want_a,
        "site A's graph holds something else: {found_a:?}"
    );
    let found_b = until(b, &query(ORDER_T, &ids.order_b, "in", 2, &[]), "B's order with its receipt", |x| x.nodes == want_b).await?;
    ensure!(
        found_b.nodes == want_b,
        "site B's graph holds something else: {found_b:?}"
    );
    let chain_b = lineage_until(b, "chain", ORDER_T, &ids.order_b, None, |l| l.ids() == want_b).await?;
    ensure!(
        chain_b.ids() == Answer::set(&[&ids.order_b, &ids.receipt_b]),
        "site B's lineage holds something else: {chain_b:?}"
    );

    // A record of one site, asked about on the other: the answer of an unknown record.
    let unknown = ask(b, &query(ORDER_T, UNKNOWN_ID, "in", 2, &[])).await?;
    unknown.expect(404)?.expect_error("graph.not_found")?;
    for (client, doctype, foreign, who) in [
        (b, ORDER_T, &ids.order_a, "B asking for an order of A"),
        (b, RECEIPT_T, &ids.receipt_a, "B asking for a receipt of A"),
        (a, ORDER_T, &ids.order_b, "A asking for an order of B"),
    ] {
        let reply = ask(client, &query(doctype, foreign, "both", 2, &[])).await?;
        reply.expect(404)?.expect_error("graph.not_found")?;
        ensure!(
            error_of(&reply) == error_of(&unknown),
            "{who} is told more than for an unknown record: {}",
            reply.text()
        );
        let by_lineage = lineage(client, "chain", doctype, foreign, None, None).await?;
        by_lineage.expect(404)?.expect_error("graph.not_found")?;
    }

    // By list: a site lists its own receipts only.
    let listed = b.list(RECEIPT_T, "limit=100").await?;
    listed.expect(200)?;
    let rows: Vec<String> = listed
        .rows()?
        .iter()
        .filter_map(|row| row.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    ensure!(
        rows == [ids.receipt_b.clone()],
        "site B lists records that are not its own: {rows:?}"
    );

    // A session of A is no session of B.
    let token = a
        .bearer()
        .ok_or_else(|| failure!("site A's client carries no session"))?
        .to_owned();
    let crossed = host.anonymous(site_b).with_token(token);
    let reply = ask(&crossed, &query(ORDER_T, &ids.order_b, "in", 2, &[])).await?;
    ensure!(
        reply.status == 401 || reply.status == 403,
        "a session of site A answers on site B with HTTP {}: {}",
        reply.status,
        reply.text()
    );
    Ok(())
}
