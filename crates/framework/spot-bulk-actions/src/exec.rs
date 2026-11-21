//! One target: the engine call, or in a dry run the read-only look.

use spot_app_api::ErrorClass;
use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::document::{DocState, Document};
use spot_app_api::error::{AppError, AppResult};
use spot_app_api::prelude::Version;

use crate::types::{BulkAction, BulkOptions, BulkStatus, BulkTarget, SkipReason};

/// The engine's failure, carried over untouched.
pub(crate) fn failed(error: &AppError) -> BulkStatus {
    BulkStatus::Failed {
        code: error.code.clone(),
        class: error.class,
        message: error.message.clone(),
    }
}

/// A failure the crate itself reports (a stale `expected` found in a dry run, a wrong state).
/// The text is fixed: it names no document and no value.
fn own_failure(class: ErrorClass, message: &str) -> BulkStatus {
    BulkStatus::Failed {
        code: class.api_code().to_owned(),
        class,
        message: message.to_owned(),
    }
}

/// The skip reason when `state` is already the state `action` would produce.
fn already_done(action: &BulkAction, state: Option<DocState>) -> Option<SkipReason> {
    match (action, state) {
        (BulkAction::Post, Some(DocState::Posted)) => Some(SkipReason::AlreadyPosted),
        (BulkAction::Cancel, Some(DocState::Cancelled)) => Some(SkipReason::AlreadyCancelled),
        _ => None,
    }
}

/// Runs one target to a status. Never panics and never returns an error: every problem is a
/// per-document [`BulkStatus::Failed`].
pub(crate) async fn run_one(
    service: &dyn DocumentService,
    inv: &Invocation,
    action: &BulkAction,
    target: &BulkTarget,
    options: &BulkOptions,
) -> BulkStatus {
    if options.dry_run {
        return dry_run(service, inv, action, target, options).await;
    }
    let doctype = target.doctype.as_str();
    let id = target.id;
    let mut loaded: Option<Document> = None;
    let expected = match target.expected {
        Some(version) => version,
        None => match service.load(inv, doctype, id).await {
            Ok(outcome) => {
                let doc = outcome.value;
                match doc.version() {
                    Some(version) => {
                        loaded = Some(doc);
                        version
                    }
                    None => {
                        return own_failure(
                            ErrorClass::Internal,
                            "the document has no stored version",
                        );
                    }
                }
            }
            Err(error) => return failed(&error),
        },
    };
    let result: AppResult<Option<Version>> = match action {
        BulkAction::Update(patch) => service
            .update(inv, doctype, id, patch.clone(), expected)
            .await
            .map(|o| o.value.version()),
        BulkAction::Post => service
            .post(inv, doctype, id, expected)
            .await
            .map(|o| o.value.version()),
        BulkAction::Cancel => service
            .cancel(inv, doctype, id, expected)
            .await
            .map(|o| o.value.version()),
        BulkAction::Correct => service
            .correct(inv, doctype, id, expected)
            .await
            .map(|o| o.value.version()),
        BulkAction::Delete => service
            .delete(inv, doctype, id, expected)
            .await
            .map(|_| None),
        BulkAction::Method { name, args } => service
            .call_method(inv, doctype, id, name, args.clone(), Some(expected))
            .await
            .map(|o| o.value.0.version()),
    };
    match result {
        Ok(version) => BulkStatus::Succeeded { version },
        Err(error) => {
            // The state is read only after a refusal, so a re-run (which finds the document
            // already changed) still reaches the engine and gets its ledgered first result.
            if options.skip_already_done && error.class == ErrorClass::InvalidState {
                let state = match &loaded {
                    Some(doc) => doc.state(),
                    None => service
                        .load(inv, doctype, id)
                        .await
                        .ok()
                        .and_then(|o| o.value.state()),
                };
                if let Some(reason) = already_done(action, state) {
                    return BulkStatus::Skipped { reason };
                }
            }
            failed(&error)
        }
    }
}

/// The dry run: one `load`, no write method.
async fn dry_run(
    service: &dyn DocumentService,
    inv: &Invocation,
    action: &BulkAction,
    target: &BulkTarget,
    options: &BulkOptions,
) -> BulkStatus {
    let doc = match service.load(inv, &target.doctype, target.id).await {
        Ok(outcome) => outcome.value,
        Err(error) => return failed(&error),
    };
    if let (Some(expected), Some(version)) = (target.expected, doc.version())
        && expected != version
    {
        return own_failure(
            ErrorClass::StaleVersion,
            "the document changed since it was read",
        );
    }
    let state = doc.state();
    if options.skip_already_done
        && let Some(reason) = already_done(action, state)
    {
        return BulkStatus::Skipped { reason };
    }
    let allowed = match action {
        // A posted document takes only the fields the DocType allows; the dry run cannot tell
        // which, so it does not refuse here.
        BulkAction::Update(_) | BulkAction::Method { .. } => true,
        BulkAction::Post => state == Some(DocState::Draft),
        BulkAction::Cancel | BulkAction::Correct => state == Some(DocState::Posted),
        BulkAction::Delete => matches!(state, None | Some(DocState::Draft)),
    };
    if allowed {
        BulkStatus::WouldSucceed
    } else {
        own_failure(
            ErrorClass::InvalidState,
            "the action is not allowed in the document's state",
        )
    }
}
