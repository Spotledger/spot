//! The right each `#[whitelist]` declares reaches the generated `MethodDecl` (WP-DOC-11.T3):
//! the engine authorises a call by it, so a macro that dropped it would silently weaken the
//! method to `write` or `read`.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::NoteController;
use spot_app_api::prelude::Name;
use spot_app_api::{AppResult, MethodContext, MethodRight};
use spot_controllers_macros::controller;

struct Rights;

#[controller(doctype = "note")]
impl Rights {
    #[whitelist(right = "read")]
    async fn m_read(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }

    #[whitelist(right = "write")]
    async fn m_write(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }

    #[whitelist(right = "create")]
    async fn m_create(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }

    #[whitelist(right = "submit")]
    async fn m_submit(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }

    #[whitelist(right = "cancel")]
    async fn m_cancel(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }

    #[whitelist(permission = "approve_note")]
    async fn m_permission(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }

    /// No arguments to the attribute: the default right is `write`.
    #[whitelist]
    async fn m_default(&self, _ctx: &mut dyn MethodContext) -> AppResult<()> {
        Ok(())
    }
}

fn right_of(decls: &[spot_app_api::MethodDecl], name: &str) -> (MethodRight, bool) {
    let decl = decls
        .iter()
        .find(|d| d.name.as_str() == name)
        .unwrap_or_else(|| panic!("no method `{name}` generated"));
    (decl.right.clone(), decl.mutates)
}

#[test]
fn every_generated_declaration_carries_the_declared_right() {
    let decls = Rights::whitelisted_methods();
    assert_eq!(decls.len(), 7, "no method is dropped");
    let approve = MethodRight::Permission(Name::new("approve_note").unwrap());
    let expected = [
        ("m_read", MethodRight::Read, false),
        ("m_write", MethodRight::Write, true),
        ("m_create", MethodRight::Create, true),
        ("m_submit", MethodRight::Submit, true),
        ("m_cancel", MethodRight::Cancel, true),
        ("m_permission", approve, true),
        ("m_default", MethodRight::Write, true),
    ];
    for (name, right, mutates) in expected {
        assert_eq!(right_of(&decls, name), (right, mutates), "method `{name}`");
    }
    // The macro never produces a declaration that contradicts itself.
    for decl in &decls {
        assert!(!decl.right.contradicts(decl.mutates), "{}", decl.name);
    }
}

#[test]
fn the_sample_controller_declares_its_rights() {
    let decls = NoteController::whitelisted_methods();
    assert_eq!(
        right_of(&decls, "peek_title"),
        (MethodRight::Read, false),
        "a read-only method"
    );
    assert_eq!(right_of(&decls, "rename"), (MethodRight::Write, true));
    let (right, _) = right_of(&decls, "approve");
    assert_eq!(right.permission().map(Name::as_str), Some("approve_note"));
}
