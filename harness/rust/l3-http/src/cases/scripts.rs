//! Server Scripts (WP-EXT-06.T4) over the real stack: an administrator saves a TypeScript
//! `validate` script for a DocType and it takes effect on the next request, with no restart.
//!
//! A registered case runs after the shutdown case of the hand-written block, so it starts a host
//! of its own. Scripts default to on, so no `common.toml` change is needed.

use serde_json::{Value as Json, json};

use super::{CaseDef, CaseFuture};
use crate::client::Reply;
use crate::fixture::Fixture;
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After the SCIM case; the case owns its fixture.
pub const ORDER: u32 = 33;

const SECTION: &str = "Server scripts";

/// Rejects a Project whose title starts with `FORBIDDEN`, with a field error on `title`.
const REJECT_FORBIDDEN: &str = r#"
export function on_validate({ doc }) {
  if (typeof doc.title === "string" && doc.title.startsWith("FORBIDDEN")) {
    host.throw({
      code: "l3.title_forbidden",
      message: "This title is not allowed.",
      fields: [{ field: "title", code: "l3.title_forbidden", message: "This title is not allowed." }],
    });
  }
}"#;

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!("the scripts case could not start its own fixture"));
        };
        let result =
            a_validate_script_blocks_a_write_on_the_next_request_and_bad_scripts_cannot_be_saved(
                &own,
            )
            .await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "scripts: a_validate_script_blocks_a_write_on_the_next_request_and_bad_scripts_cannot_be_saved",
        SECTION,
        "a saved `validate` script refuses a Project create with a 422 field error on `title` at once (a title without the marker still saves), disabling it lets the same create through, a source that does not compile is refused on `script` with its line and column, a script for a reserved DocType is refused with `serverscript.record_invalid`, and a Viewer cannot create a script",
        false,
        run_case,
    )]
}

fn script_body(name: &str, doctype: &str, source: &str) -> Json {
    json!({
        "name": name,
        "doctype": doctype,
        "event": "validate",
        "script": source,
        "enabled": true,
    })
}

fn field_errors(reply: &Reply) -> Vec<Json> {
    reply
        .json()
        .pointer("/error/fields")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default()
}

async fn project_with_title(
    fixture: &Fixture,
    title: &str,
) -> Result<Reply, crate::report::Failure> {
    fixture
        .admin()
        .create(
            "project",
            &json!({
                "code": fixture.unique("PRJ"),
                "title": title,
                "status": "Active",
                "owner": fixture.admin_id(),
                "currency": "USD",
                "budget": "10.00",
            }),
        )
        .await
}

async fn a_validate_script_blocks_a_write_on_the_next_request_and_bad_scripts_cannot_be_saved(
    fixture: &Fixture,
) -> CaseResult {
    let admin = fixture.admin();
    let name = fixture.unique("l3_script");

    // 1. Before the script exists the marked title saves.
    let before = project_with_title(fixture, "FORBIDDEN before").await?;
    before.expect(201)?;

    // 2. A valid script saves with its compiled hash.
    let saved = admin
        .create(
            "server_script",
            &script_body(&name, "project", REJECT_FORBIDDEN),
        )
        .await?;
    saved.expect(201)?;
    ensure!(
        saved
            .field("compiled_hash")
            .as_str()
            .is_some_and(|h| !h.is_empty()),
        "the saved script carries no compiled_hash: {}",
        saved.text()
    );
    let script_id = saved.id()?;

    // 3. The next request is refused, on the field the script named; a clean title still saves.
    let refused = project_with_title(fixture, "FORBIDDEN after").await?;
    ensure!(
        refused.status == 422,
        "the create answered HTTP {}, not 422: {}",
        refused.status,
        refused.text()
    );
    ensure!(
        field_errors(&refused)
            .iter()
            .any(|f| f["field"] == json!("title") && f["code"] == json!("l3.title_forbidden")),
        "the refusal does not carry the script's field error: {}",
        refused.text()
    );
    project_with_title(fixture, "Allowed title")
        .await?
        .expect(201)?;

    // 4. Disabling the script lets the same create through, again with no restart. The refusal
    // above was a run the host recorded on the script (`last_error`), which is a write: read the
    // version the script has now, as any editor does.
    let current = admin.document("server_script", &script_id).await?;
    current.expect(200)?;
    let version = current.version()?;
    admin
        .update(
            "server_script",
            &script_id,
            version,
            &json!({"enabled": false}),
        )
        .await?
        .expect(200)?;
    project_with_title(fixture, "FORBIDDEN disabled")
        .await?
        .expect(201)?;

    // 5. A source that does not compile cannot be saved; the diagnostic names line and column.
    let broken = admin
        .create(
            "server_script",
            &script_body(
                &fixture.unique("l3_broken"),
                "project",
                "export function on_validate({ doc }) {\n  let = ;\n}",
            ),
        )
        .await?;
    ensure!(
        broken.status == 422,
        "a script that does not compile answered HTTP {}, not 422: {}",
        broken.status,
        broken.text()
    );
    ensure!(
        field_errors(&broken).iter().any(|f| {
            f["field"] == json!("script")
                && f["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("line") && m.contains("column"))
        }),
        "the compile refusal does not name line and column on `script`: {}",
        broken.text()
    );

    // 6. A script for a DocType that holds credentials is refused.
    let reserved = admin
        .create(
            "server_script",
            &script_body(
                &fixture.unique("l3_reserved"),
                "session",
                "export function on_validate({ doc }) {}",
            ),
        )
        .await?;
    ensure!(
        field_errors(&reserved)
            .iter()
            .any(|f| f["code"] == json!("serverscript.record_invalid")),
        "a script for `session` was not refused with serverscript.record_invalid: {}",
        reserved.text()
    );

    // 7. A user without the right on `server_script` cannot create one: no role holds it by default.
    let viewer = fixture.sign_in(crate::fixture::VIEWER).await?;
    let denied = viewer
        .create(
            "server_script",
            &script_body(&fixture.unique("l3_denied"), "project", REJECT_FORBIDDEN),
        )
        .await?;
    ensure!(
        denied.status == 403,
        "a Viewer's script create answered HTTP {}, not 403: {}",
        denied.status,
        denied.text()
    );
    Ok(())
}
