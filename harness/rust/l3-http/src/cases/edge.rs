//! Metadata, idempotency, request ids, health, unknown hosts, the body limit and shutdown.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::fixture::Fixture;
use crate::report::CaseResult;
use crate::{ensure, failure};

/// Metadata describes every declared field and carries the version as its `ETag`; a matching
/// `If-None-Match` answers 304.
pub async fn meta_describes_every_field_and_its_etag(fixture: &Fixture) -> CaseResult {
    let meta = fixture.admin().meta("task").await?;
    meta.expect(200)?;
    let names: Vec<String> = meta
        .json()
        .pointer("/data/fields")
        .and_then(|f| f.as_array())
        .map(|fields| {
            fields
                .iter()
                .filter_map(|f| f["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    for expected in ["subject", "project", "checklist", "details", "related_to"] {
        ensure!(
            names.iter().any(|name| name == expected),
            "the task metadata does not declare `{expected}`: {names:?}"
        );
    }
    ensure!(
        meta.json().pointer("/data/is_posted") == Some(&json!(false)),
        "the task metadata says the DocType is posted: {}",
        meta.text()
    );
    let etag = meta.etag()?;
    let cached = fixture
        .admin()
        .send(
            "GET",
            "/api/v1/meta/task",
            &[("If-None-Match", &etag)],
            None,
        )
        .await?;
    ensure!(
        cached.status == 304,
        "the matching If-None-Match answers HTTP {}, not 304: {}",
        cached.status,
        cached.text()
    );
    ensure!(
        cached.body.is_empty(),
        "the 304 carries a body: {}",
        cached.text()
    );
    Ok(())
}

/// A field added to the site's copy of a DocType is visible in metadata and writable after a
/// reload, without restarting the host.
pub async fn a_metadata_change_is_visible_without_a_restart(fixture: &Fixture) -> CaseResult {
    let doctype = fixture.root().join("sample/doctypes/task.toml");
    let text = std::fs::read_to_string(&doctype)
        .map_err(|e| failure!("cannot read {}: {e}", doctype.display()))?;
    std::fs::write(
        &doctype,
        format!(
            "{text}\n[[fields]]\nname = \"probe_note\"\nlabel = \"Probe Note\"\ntype = \"text\"\nmax_chars = 40\n"
        ),
    )
    .map_err(|e| failure!("cannot write {}: {e}", doctype.display()))?;
    spot_cli::request_reload(fixture.root(), fixture.site())
        .map_err(|e| failure!("cannot request a reload: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let meta = fixture.admin().meta("task").await?;
        let present = meta
            .json()
            .pointer("/data/fields")
            .and_then(|f| f.as_array())
            .is_some_and(|fields| fields.iter().any(|f| f["name"] == json!("probe_note")));
        if present {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "the reloaded metadata never named `probe_note`: {}",
            meta.text()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let (project_id, _, _) = fixture.project("Reloaded field").await?;
    let created = fixture
        .admin()
        .create(
            "task",
            &json!({"subject": "After reload", "project": project_id,
                    "probe_note": "written after the reload"}),
        )
        .await?;
    created.expect(201)?;
    ensure!(
        created.field("probe_note") == json!("written after the reload"),
        "the new field did not round-trip: {}",
        created.text()
    );
    Ok(())
}

/// The same `Idempotency-Key` and body return the same document, marked as a replay.
pub async fn an_idempotency_key_returns_the_same_document(fixture: &Fixture) -> CaseResult {
    let key = fixture.unique("idem");
    let body = json!({"code": fixture.unique("PRJ"), "title": "Idempotent",
                      "owner": fixture.admin_id()});
    let first = fixture
        .admin()
        .create_with_key("project", &key, &body)
        .await?;
    first.expect(201)?;
    let second = fixture
        .admin()
        .create_with_key("project", &key, &body)
        .await?;
    second.expect(201)?;
    ensure!(
        second.id()? == first.id()?,
        "the replay created another document: {} vs {}",
        first.id()?,
        second.id()?
    );
    ensure!(
        second.header("idempotency-replayed").is_some(),
        "the replay is not marked with `Idempotency-Replayed`: {:?}",
        second.headers
    );
    ensure!(
        second.header("location") == first.header("location"),
        "the replay carries another Location: {:?} vs {:?}",
        first.header("location"),
        second.header("location")
    );
    Ok(())
}

/// A valid `X-Request-Id` is echoed; an invalid one is replaced; an error carries it.
pub async fn the_request_id_round_trips(fixture: &Fixture) -> CaseResult {
    let echoed = fixture
        .admin()
        .send(
            "GET",
            "/api/v1/meta/task",
            &[("X-Request-Id", "req-l3-http-1")],
            None,
        )
        .await?;
    echoed.expect(200)?;
    ensure!(
        echoed.header("x-request-id") == Some("req-l3-http-1"),
        "a valid request id is not echoed: {:?}",
        echoed.header("x-request-id")
    );
    let replaced = fixture
        .admin()
        .send(
            "GET",
            "/api/v1/meta/task",
            &[("X-Request-Id", "not a valid id")],
            None,
        )
        .await?;
    replaced.expect(200)?;
    ensure!(
        replaced.header("x-request-id") != Some("not a valid id"),
        "an invalid request id is echoed unchanged"
    );
    ensure!(
        replaced
            .header("x-request-id")
            .is_some_and(|id| !id.is_empty()),
        "the replaced request id is missing"
    );
    let error = fixture
        .client()
        .anonymous()
        .send(
            "GET",
            "/api/v1/meta/task",
            &[("X-Request-Id", "req-l3-http-error")],
            None,
        )
        .await?;
    ensure!(
        error.request_id().as_deref() == Some("req-l3-http-error"),
        "the error does not carry the caller's request id: {}",
        error.text()
    );
    Ok(())
}

/// Liveness and readiness answer, and readiness names its storage check.
pub async fn the_health_endpoints_answer(fixture: &Fixture) -> CaseResult {
    let live = fixture.client().health("live").await?;
    live.expect(200)?;
    ensure!(
        live.json().get("status") == Some(&json!("live")),
        "liveness answered {}",
        live.text()
    );
    let ready = fixture.client().health("ready").await?;
    ready.expect(200)?;
    ensure!(
        ready.json().get("status") == Some(&json!("ready")),
        "readiness answered {}",
        ready.text()
    );
    let checks = ready
        .json()
        .get("checks")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    ensure!(
        checks.iter().any(
            |check| check["name"] == json!(format!("storage:{}", fixture.site()))
                && check["ok"] == json!(true)
        ),
        "readiness does not name a passing storage check: {}",
        ready.text()
    );
    Ok(())
}

/// A request for a site the host does not serve is the `http.unknown_site` envelope.
pub async fn an_unknown_site_is_a_404_envelope(fixture: &Fixture) -> CaseResult {
    let nowhere = crate::client::Client::new(fixture.addr(), "nowhere.invalid");
    let reply = nowhere.meta("task").await?;
    ensure!(
        reply.status == 404,
        "an unknown site answers HTTP {}, not 404: {}",
        reply.status,
        reply.text()
    );
    ensure!(
        reply.error_code().as_deref() == Some("http.unknown_site"),
        "an unknown site answers `{:?}`, not `http.unknown_site`: {}",
        reply.error_code(),
        reply.text()
    );
    Ok(())
}

/// A body over the configured limit is the `http.body_too_large` envelope. The declared
/// `Content-Length` is refused before a byte of the body is read, so this case sends the head
/// only (a 10 MiB body would otherwise make the client see the server's close as a broken pipe).
pub async fn a_body_over_the_limit_is_a_413_envelope(fixture: &Fixture) -> CaseResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The host's default limit is 10 MiB.
    let declared = 10 * 1024 * 1024 + 64;
    let head = format!(
        "POST /api/v1/resource/project HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n",
        fixture.host()
    );
    let mut stream = tokio::net::TcpStream::connect(fixture.addr())
        .await
        .map_err(|e| failure!("cannot connect: {e}"))?;
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| failure!("cannot write the head: {e}"))?;
    stream.flush().await.ok();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .map_err(|_| failure!("the 413 never arrived"))?
        .map_err(|e| failure!("cannot read the 413: {e}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    ensure!(
        text.starts_with("HTTP/1.1 413"),
        "an oversized declared body answers `{}`",
        text.lines().next().unwrap_or_default()
    );
    ensure!(
        text.contains("http.body_too_large"),
        "the 413 does not carry `http.body_too_large`: {text}"
    );
    Ok(())
}

/// Shutdown drains a request already in flight: a request whose body is half sent when the
/// shutdown begins completes, and the port is closed afterwards.
pub async fn a_shutdown_drains_the_requests_in_flight(fixture: &Fixture) -> CaseResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let admin = fixture.admin();
    let body = json!({
        "code": fixture.unique("PRJ-SHUT"),
        "title": "In flight",
        "owner": fixture.admin_id(),
    })
    .to_string();
    let split = body.len() / 2;
    let head = format!(
        "POST /api/v1/resource/project HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        fixture.host(),
        admin.bearer().unwrap_or_default(),
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(fixture.addr())
        .await
        .map_err(|e| failure!("cannot connect: {e}"))?;
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| failure!("cannot write the head: {e}"))?;
    stream
        .write_all(&body.as_bytes()[..split])
        .await
        .map_err(|e| failure!("cannot write the first half: {e}"))?;
    stream.flush().await.ok();
    // The handler is now reading the body: this request is in flight.
    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture.begin_shutdown();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stream
        .write_all(&body.as_bytes()[split..])
        .await
        .map_err(|e| failure!("the server closed the in-flight request: {e}"))?;
    stream.flush().await.ok();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .map_err(|_| failure!("the in-flight request never completed"))?
        .map_err(|e| failure!("cannot read the in-flight answer: {e}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    ensure!(
        text.starts_with("HTTP/1.1 201"),
        "the in-flight request was not drained: `{}`",
        text.lines().next().unwrap_or_default()
    );
    fixture.join_host().await;
    let after = fixture.client().health("live").await;
    ensure!(
        after.is_err(),
        "the host still answers after shutdown: {:?}",
        after.map(|reply| reply.status)
    );
    Ok(())
}
