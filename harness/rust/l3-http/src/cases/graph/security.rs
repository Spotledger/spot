//! The security regressions of the graph routes over HTTP: a body cannot name a tenant or a
//! principal, injection strings are data, no document value leaves in an answer or a file, and
//! the external projection is off by default and creates nothing.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::answer::{Answer, answer, ask, query, until};
use super::host::{FAST, GraphHost, LINEAGE, ORDER_T, RECEIPT_T, Start};
use crate::client::Client;
use crate::report::CaseResult;
use crate::{ensure, failure};

/// A document value no answer and no file may carry.
const MARKER: &str = "LEAK-MARKER-8a41c9";

pub async fn the_graph_routes_refuse_forged_bodies_and_injection() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        lineage: Some(LINEAGE),
        ..Start::default()
    })
    .await?;
    let result = run(&host).await;
    host.finish().await;
    result
}

async fn run(host: &GraphHost) -> CaseResult {
    let admin = &host.admin;
    let (order, _) = host
        .make(admin, ORDER_T, &json!({"ref": "SEC-1", "note": MARKER}))
        .await?;
    let (receipt, _) = host
        .make(admin, RECEIPT_T, &json!({"source": order, "note": MARKER}))
        .await?;
    let body = query(ORDER_T, &order, "in", 2, &[]);
    let want = Answer::set(&[&order, &receipt]);
    let baseline = until(admin, &body, "the order with its receipt", |a| a.nodes == want).await?;
    ensure!(
        baseline.nodes == Answer::set(&[&order, &receipt]),
        "the baseline answer is not the order and its receipt: {baseline:?}"
    );

    forged_bodies(admin, &order, &body, &baseline).await?;
    injection(host, admin, &order, &body, &baseline).await?;
    no_values(host, admin, &order).await?;
    projection_is_off(host).await
}

/// A body that names a tenant or a principal is refused as a whole; a header that names another
/// site changes nothing.
async fn forged_bodies(
    admin: &Client,
    order: &str,
    body: &serde_json::Value,
    baseline: &Answer,
) -> CaseResult {
    for key in [
        "tenant",
        "tenant_id",
        "site",
        "principal",
        "user",
        "as_user",
    ] {
        let mut forged = body.clone();
        forged[key] = json!("EVIL-VALUE-77");
        let reply = ask(admin, &forged).await?;
        reply.expect(422)?.expect_error("graph.query_invalid")?;
        ensure!(
            !reply.text().contains("EVIL-VALUE-77"),
            "the refusal of `{key}` echoes the submitted value: {}",
            reply.text()
        );
        let mut lineage_body = json!({"kind": "chain", "doctype": ORDER_T, "id": order});
        lineage_body[key] = json!("EVIL-VALUE-77");
        let reply = admin
            .send_json("POST", "/api/v1/lineage", &[], &lineage_body)
            .await?;
        reply.expect(422)?.expect_error("graph.query_invalid")?;
        ensure!(
            !reply.text().contains("EVIL-VALUE-77"),
            "the lineage refusal of `{key}` echoes the submitted value: {}",
            reply.text()
        );
    }
    for (name, value) in [
        ("X-Tenant", "some-other-site"),
        ("X-Spot-Site", "some-other-site"),
        ("X-Forwarded-Host", "some-other-site"),
    ] {
        let reply = admin
            .send_json("POST", "/api/v1/graph/query", &[(name, value)], body)
            .await?;
        let same = Answer::from(&reply)?;
        ensure!(
            &same == baseline,
            "the header `{name}` changed the answer: {same:?}"
        );
    }
    Ok(())
}

/// Hostile strings are data: refused as invalid or answered as not found, never a server error,
/// never an effect on the edge table.
async fn injection(
    host: &GraphHost,
    admin: &Client,
    order: &str,
    body: &serde_json::Value,
    baseline: &Answer,
) -> CaseResult {
    let hostile = [
        "'; DROP TABLE spot_graph_edge; --",
        "1 OR 1=1",
        "../../etc/passwd",
        "chain_order\"; DROP TABLE x; --",
        "%00%0d%0aX-Injected: 1",
    ];
    let before = host
        .count(host.site(), "SELECT count(*) FROM spot_graph_edge")
        .await?;
    for text in hostile {
        // As the start's id and as its DocType.
        for (doctype, id) in [(ORDER_T, text), (text, order)] {
            let reply = ask(admin, &query(doctype, id, "in", 2, &[])).await?;
            ensure!(
                (400..500).contains(&reply.status),
                "`{text}` as a start gave HTTP {}: {}",
                reply.status,
                reply.text()
            );
            ensure!(
                matches!(
                    reply.error_code().as_deref(),
                    Some("graph.query_invalid" | "graph.not_found")
                ),
                "`{text}` as a start gave the error {:?}",
                reply.error_code()
            );
            ensure!(
                !reply.text().contains(text),
                "the refusal echoes the hostile string: {}",
                reply.text()
            );
        }
        // As an edge type, a filter field and a filter value: refused, or answered as data.
        let mut as_type = query(ORDER_T, order, "in", 2, &[text]);
        as_type["limit"] = json!(10);
        let mut as_field = body.clone();
        as_field["node_filter"] = json!({"field": text, "op": "eq", "value": "x"});
        let mut as_value = body.clone();
        as_value["node_filter"] = json!({"field": "note", "op": "eq", "value": text});
        for (what, hostile_body) in [
            ("an edge type", as_type),
            ("a filter field", as_field),
            ("a filter value", as_value),
        ] {
            let reply = ask(admin, &hostile_body).await?;
            ensure!(
                reply.status < 500,
                "`{text}` as {what} gave HTTP {}: {}",
                reply.status,
                reply.text()
            );
            if reply.status == 200 {
                let found = Answer::from(&reply)?;
                ensure!(
                    found.nodes.is_subset(&baseline.nodes),
                    "`{text}` as {what} widened the answer: {found:?}"
                );
            }
            ensure!(
                !reply.text().contains(text),
                "the answer to `{text}` as {what} echoes it: {}",
                reply.text()
            );
        }
    }
    // The table and the graph are as they were.
    let after = host
        .count(host.site(), "SELECT count(*) FROM spot_graph_edge")
        .await?;
    ensure!(
        after == before,
        "the edge table changed: {before} rows, then {after}"
    );
    let again = answer(admin, body).await?;
    ensure!(
        &again == baseline,
        "the graph changed under hostile input: {again:?}"
    );
    Ok(())
}

/// A document value is in no answer and in no file the site writes.
async fn no_values(host: &GraphHost, admin: &Client, order: &str) -> CaseResult {
    let graph = ask(admin, &query(ORDER_T, order, "both", 3, &[])).await?;
    graph.expect(200)?;
    let by_lineage = super::answer::lineage(admin, "chain", ORDER_T, order, None, None).await?;
    by_lineage.expect(200)?;
    for (what, text) in [
        ("graph_query", graph.text()),
        ("lineage_trace", by_lineage.text()),
    ] {
        ensure!(
            !text.contains(MARKER),
            "the answer of {what} carries a document value: {text}"
        );
    }
    // The one place the marker is allowed is the document itself.
    let document = admin.document(ORDER_T, order).await?;
    ensure!(
        document.text().contains(MARKER),
        "the marker is not in the document the cases wrote: {}",
        document.text()
    );
    let sites = host.fixture.root().join("sites");
    if let Some(found) = find_marker(&sites, MARKER) {
        return Err(failure!(
            "a document value is in a file the site wrote: {}",
            found.display()
        ));
    }
    if let Ok(log) = std::fs::read_to_string(host.fixture.log()) {
        ensure!(!log.contains(MARKER), "a document value is in the run log");
    }
    Ok(())
}

fn find_marker(dir: &Path, marker: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_marker(&path, marker) {
                return Some(found);
            }
        } else if let Ok(bytes) = std::fs::read(&path)
            && String::from_utf8_lossy(&bytes).contains(marker)
        {
            return Some(path);
        }
    }
    None
}

/// The projection to an external engine is off unless a site turns it on: no cursor, no table.
async fn projection_is_off(host: &GraphHost) -> CaseResult {
    let cursor = host
        .fixture
        .root()
        .join("sites")
        .join(host.site())
        .join("graph_projection.cursor");
    ensure!(
        !cursor.exists(),
        "the projection left a cursor behind: {}",
        cursor.display()
    );
    let tables = host
        .count(
            host.site(),
            "SELECT count(*) FROM information_schema.tables WHERE table_name LIKE '%graph_projection%'",
        )
        .await?;
    ensure!(
        tables == 0,
        "the projection created {tables} table(s) while off"
    );
    Ok(())
}
