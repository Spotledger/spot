//! A small stub catalogue and handlers: the operations of the tests and of the `catalogue_json`
//! example. The real operations are WP-ADM-01.T6 and the migration verbs.

use serde_json::json;
use spot_mgmt_ops::{Catalogue, OpId, OperationSpec, ParamSpec, ParamType, Risk, TargetKind};

use crate::handlers::{HandlerRegistry, handler_fn};

fn text(name: &str, pattern: &str, max_len: usize, secret: bool) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: Some(pattern.to_owned()),
        },
        required: true,
        default: None,
        doc: format!("The {name}."),
        secret,
    }
}

fn site() -> ParamSpec {
    text("site", "[a-z][a-z0-9-]{1,62}", 63, false)
}

fn operation(id: &str, risk: Risk, params: Vec<ParamSpec>) -> Option<OperationSpec> {
    Some(OperationSpec {
        id: OpId::try_new(id).ok()?,
        title: format!("Stub {id}"),
        summary: format!("The stub operation {id}."),
        params,
        risk,
        target: TargetKind::Server,
    })
}

/// The ids of the stub operations, by risk: `site.status` (read), `site.backup.run` (medium),
/// `site.migrate.apply` (high), `site.maintenance.enter` and `site.restore.apply` (critical).
#[must_use]
pub fn stub_catalogue() -> Option<Catalogue> {
    let mut builder = Catalogue::builder();
    builder.add("site", operation("site.status", Risk::Read, vec![site()])?);
    builder.add(
        "site",
        operation(
            "site.backup.run",
            Risk::Medium,
            vec![site(), text("target", "[a-z][a-z0-9_-]{0,99}", 100, false)],
        )?,
    );
    builder.add(
        "site",
        operation(
            "site.migrate.apply",
            Risk::High,
            vec![site(), text("plan_hash", "[0-9a-f]{64}", 64, false)],
        )?,
    );
    builder.add(
        "site",
        operation(
            "site.maintenance.enter",
            Risk::Critical,
            vec![site(), text("reason", ".{1,500}", 500, false)],
        )?,
    );
    builder.add(
        "site",
        operation(
            "site.restore.apply",
            Risk::Critical,
            vec![site(), text("confirm", "[A-Za-z0-9]{8,64}", 64, true)],
        )?,
    );
    builder.build().ok()
}

/// Handlers for the stub operations: each answers at once with a fixed small result.
#[must_use]
pub fn stub_handlers() -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    for id in [
        "site.status",
        "site.backup.run",
        "site.migrate.apply",
        "site.maintenance.enter",
        "site.restore.apply",
    ] {
        let _ = registry.register(
            id,
            handler_fn(|_context, _request| async { Ok(json!({ "state": "ok" })) }),
            false,
        );
    }
    registry
}
