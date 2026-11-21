//! The hand-built DocType definitions the stub-host tests define on the host.
//!
//! The shapes match the embedded TOML text field-for-field; the detached
//! validation crate asserts exactly that against the `spot_meta` load. The stub
//! host tests compile against these; the app itself does not call them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::prelude::Name;
use spot_app_api::{DecimalFormat, DocTypeDef, FieldDef, FieldKind, LongTextFormat, TextFormat};

/// A text field.
fn text(name: &str, required: bool, max_chars: u32) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Text {
            max_chars: Some(max_chars),
            format: TextFormat::Plain,
        },
        required,
        false,
    )
}

/// A link field.
fn link(name: &str, target: &str, required: bool) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Link {
            doctype: Name::new(target).unwrap(),
        },
        required,
        false,
    )
}

/// A decimal field, scale 2, optionally read-only.
fn decimal_field(name: &str, required: bool, read_only: bool) -> FieldDef {
    let mut f = FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Decimal {
            scale: 2,
            precision: 18,
            format: DecimalFormat::Plain,
        },
        required,
        false,
    );
    f.read_only = read_only;
    f
}

/// A date field.
fn date(name: &str, required: bool) -> FieldDef {
    FieldDef::new(Name::new(name).unwrap(), FieldKind::Date, required, false)
}

/// A select field.
fn select(name: &str, values: &[&str], required: bool) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Select {
            values: values.iter().map(|s| (*s).to_owned()).collect(),
        },
        required,
        false,
    )
}

/// A bool field.
fn bool_field(name: &str) -> FieldDef {
    FieldDef::new(Name::new(name).unwrap(), FieldKind::Bool, false, false)
}

/// A long-text field.
fn long_text(name: &str) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::LongText {
            format: LongTextFormat::Plain,
        },
        false,
        false,
    )
}

/// The `user` DocType, as the platform app ships it (the link target).
#[must_use]
pub fn user_doctype() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new("user").unwrap(),
        "User".to_owned(),
        false,
        false,
        Some(Name::new("name").unwrap()),
        vec![text("name", true, 140)],
    );
    def.version = 1;
    def
}

/// The `time_entry` DocType, as the app's TOML text compiles it.
#[must_use]
pub fn time_entry_doctype() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new("time_entry").unwrap(),
        "Time Entry".to_owned(),
        true,
        false,
        Some(Name::new("entry_no").unwrap()),
        vec![
            text("entry_no", true, 140),
            link("task", "task", true),
            link("person", "user", true),
            date("work_date", true),
            decimal_field("hours", true, false),
            bool_field("billable"),
            long_text("note"),
        ],
    );
    def.version = 1;
    def
}

/// The `task` DocType, as the app's TOML text compiles it.
#[must_use]
pub fn task_doctype() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new("task").unwrap(),
        "Task".to_owned(),
        false,
        false,
        Some(Name::new("subject").unwrap()),
        vec![
            text("subject", true, 140),
            link("project", "project", true),
            link("assignee", "user", false),
            link("project_owner", "user", false),
            select("priority", &["Low", "Medium", "High", "Urgent"], false),
            date("due_date", false),
            decimal_field("estimated_hours", false, false),
            decimal_field("logged_hours", false, true),
            select(
                "status",
                &["Open", "In Progress", "Blocked", "Review", "Done"],
                true,
            ),
            long_text("details"),
            long_text("blocked_reason"),
        ],
    );
    def.version = 1;
    def
}

/// The `project` DocType, as the app's TOML text compiles it.
#[must_use]
pub fn project_doctype() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new("project").unwrap(),
        "Project".to_owned(),
        false,
        false,
        Some(Name::new("code").unwrap()),
        vec![
            text("code", true, 140),
            text("title", true, 140),
            select("status", &["Planning", "Active", "On Hold", "Done"], true),
            link("owner", "user", true),
            date("start_date", false),
            date("end_date", false),
            decimal_field("budget", false, false),
            select("currency", &["USD", "EUR", "GBP", "PKR"], false),
            long_text("description"),
        ],
    );
    def.version = 1;
    def
}

/// The `label` DocType, as the app's TOML text compiles it.
#[must_use]
pub fn label_doctype() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new("label").unwrap(),
        "Label".to_owned(),
        false,
        false,
        Some(Name::new("name").unwrap()),
        vec![text("name", true, 50), text("colour", false, 9)],
    );
    def.version = 1;
    def
}

/// The `checklist_item` child DocType.
#[must_use]
pub fn checklist_item_doctype() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new("checklist_item").unwrap(),
        "Checklist Item".to_owned(),
        false,
        true,
        None,
        vec![text("text", true, 200), bool_field("done")],
    );
    def.version = 1;
    def
}
