//! The document lifecycle: post, cancel, correct, and the rules a posted document keeps.

use crate::ensure;
use crate::fixture::Fixture;
use crate::report::CaseResult;

/// A draft Time Entry is posted, and correcting a posted entry opens an amended draft linked
/// to the original.
pub async fn correct_a_draft_amends_the_posted_entry(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, _, _) = fixture.project("Correct").await?;
    let (task_id, _) = fixture.task(&project_id, "Correct task").await?;
    let (entry_id, entry_version) = fixture.time_entry(&task_id, "1.00", "2026-10-20").await?;
    let posted = admin
        .lifecycle("time_entry", &entry_id, "post", entry_version, None)
        .await?;
    posted.expect(200)?;
    ensure!(
        posted.state()? == "posted",
        "the entry is `{}` after post, not `posted`: {}",
        posted.state()?,
        posted.text()
    );
    let amended = admin
        .lifecycle("time_entry", &entry_id, "correct", posted.version()?, None)
        .await?;
    amended.expect(201)?;
    ensure!(
        amended.state()? == "draft",
        "the amended document is `{}`, not `draft`: {}",
        amended.state()?,
        amended.text()
    );
    ensure!(
        amended.field("amended_from") == serde_json::json!(entry_id),
        "the amended draft does not name its original: {}",
        amended.text()
    );
    ensure!(
        amended.id()? != entry_id,
        "correct reused the original id instead of creating a draft"
    );
    let original = admin.document("time_entry", &entry_id).await?;
    original.expect(200)?;
    ensure!(
        original.state()? == "cancelled",
        "the original is `{}` after correct, not `cancelled`: {}",
        original.state()?,
        original.text()
    );
    Ok(())
}

/// A posted document refuses a plain edit: the fields are fixed, and the field error names
/// `document.read_only_field`.
pub async fn a_posted_document_refuses_a_plain_edit(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let (project_id, _, _) = fixture.project("Posted edit").await?;
    let (task_id, _) = fixture.task(&project_id, "Posted task").await?;
    let (entry_id, entry_version) = fixture.time_entry(&task_id, "2.00", "2026-10-21").await?;
    let posted = admin
        .lifecycle("time_entry", &entry_id, "post", entry_version, None)
        .await?;
    posted.expect(200)?;
    let reply = admin
        .update(
            "time_entry",
            &entry_id,
            posted.version()?,
            &serde_json::json!({"note": "a plain edit"}),
        )
        .await?;
    reply.expect_error("document.validation_failed")?;
    ensure!(
        reply.status == 422,
        "the posted edit answers HTTP {}, not 422: {}",
        reply.status,
        reply.text()
    );
    let field_codes: Vec<String> = reply
        .json()
        .pointer("/error/fields")
        .and_then(|f| f.as_array())
        .map(|fields| {
            fields
                .iter()
                .filter_map(|f| f["code"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    ensure!(
        field_codes
            .iter()
            .any(|code| code == "document.read_only_field"),
        "the refusal does not name `document.read_only_field`: {}",
        reply.text()
    );
    Ok(())
}
