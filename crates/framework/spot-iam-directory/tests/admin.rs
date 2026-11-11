//! Last-administrator protection on both counters.
#![allow(missing_docs)]

mod common;

use common::id;
use spot_iam_directory::{Change, ChangeKind, DirectoryView, check_admin_invariant, codes};
use spot_storage_contract::RecordId;

/// A tenant described by plain numbers and the ids of the administrators it has.
struct View {
    tenant_admins: Vec<RecordId>,
    break_glass: Vec<RecordId>,
    sso: bool,
}

impl DirectoryView for View {
    fn enabled_tenant_admins(&self) -> usize {
        self.tenant_admins.len()
    }
    fn enabled_break_glass_admins(&self) -> usize {
        self.break_glass.len()
    }
    fn is_enabled_tenant_admin(&self, user: RecordId) -> bool {
        self.tenant_admins.contains(&user)
    }
    fn is_enabled_break_glass_admin(&self, user: RecordId) -> bool {
        self.break_glass.contains(&user)
    }
    fn sso_required(&self) -> bool {
        self.sso
    }
}

const KINDS: [ChangeKind; 4] = [
    ChangeKind::Disable,
    ChangeKind::Delete,
    ChangeKind::Demote,
    ChangeKind::Expire,
];

fn codes_of(view: &View, user: RecordId, kind: ChangeKind) -> Vec<&'static str> {
    check_admin_invariant(view, &Change { user, kind })
        .err()
        .map(|e| e.codes())
        .unwrap_or_default()
}

#[test]
fn the_last_tenant_administrator_cannot_go_by_any_route() {
    let (only, other) = (id(), id());
    let view = View {
        tenant_admins: vec![only],
        break_glass: vec![],
        sso: false,
    };
    for kind in KINDS {
        assert_eq!(codes_of(&view, only, kind), [codes::LAST_ADMIN], "{kind:?}");
        // Anyone else is unaffected.
        assert!(codes_of(&view, other, kind).is_empty(), "{kind:?}");
    }
}

#[test]
fn one_of_several_tenant_administrators_may_go() {
    let (a, b) = (id(), id());
    let view = View {
        tenant_admins: vec![a, b],
        break_glass: vec![],
        sso: true,
    };
    for kind in KINDS {
        assert!(codes_of(&view, a, kind).is_empty(), "{kind:?}");
    }
}

#[test]
fn the_last_break_glass_administrator_cannot_go() {
    let (bg, admin) = (id(), id());
    let view = View {
        tenant_admins: vec![admin, bg],
        break_glass: vec![bg],
        sso: false,
    };
    for kind in KINDS {
        assert_eq!(codes_of(&view, bg, kind), [codes::LAST_ADMIN], "{kind:?}");
    }
}

#[test]
fn while_sso_is_required_two_break_glass_administrators_must_remain() {
    let (a, b, c, admin) = (id(), id(), id(), id());
    let view = |bg: Vec<RecordId>, sso| View {
        tenant_admins: vec![admin],
        break_glass: bg,
        sso,
    };
    // Three: one may go, two remain.
    assert!(codes_of(&view(vec![a, b, c], true), a, ChangeKind::Disable).is_empty());
    // Two: removing one leaves one, under the minimum.
    assert_eq!(
        codes_of(&view(vec![a, b], true), a, ChangeKind::Disable),
        [codes::BREAK_GLASS_MINIMUM]
    );
    // Without SSO required, two may become one.
    assert!(codes_of(&view(vec![a, b], false), a, ChangeKind::Disable).is_empty());
    // The last one under SSO breaks both rules, and both are reported.
    assert_eq!(
        codes_of(&view(vec![a], true), a, ChangeKind::Demote),
        [codes::LAST_ADMIN, codes::BREAK_GLASS_MINIMUM]
    );
}

#[test]
fn a_user_who_is_both_reports_each_problem_once() {
    let both = id();
    let view = View {
        tenant_admins: vec![both],
        break_glass: vec![both],
        sso: true,
    };
    assert_eq!(
        codes_of(&view, both, ChangeKind::Delete),
        [
            codes::LAST_ADMIN,
            codes::LAST_ADMIN,
            codes::BREAK_GLASS_MINIMUM
        ]
    );
}
