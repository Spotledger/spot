//! The app's names as constants (WP-DX-05.T1), so callers never repeat strings.

/// The DocType storage names, in definition order.
pub const DOCTYPE_NAMES: [&str; 6] = [
    "checklist_item",
    "label",
    "label_pick",
    "project",
    "task",
    "time_entry",
];

/// The `project` DocType's code field: named by the series `PRJ-.YYYY.-.####`.
pub const PROJECT_CODE: &str = "PRJ-.YYYY.-.####";

/// The `time_entry` entry number field: named by the series `TE-.YYYY.-.#####`.
pub const TIME_ENTRY_NUMBER: &str = "TE-.YYYY.-.#####";

/// The `task` field holding the hours the controller rolls up.
pub const TASK_LOGGED_HOURS: &str = "logged_hours";

/// The hours field of a Time Entry, in the shared decimal format.
pub const TIME_ENTRY_HOURS: &str = "hours";

/// The Task field linking its Project.
pub const TASK_PROJECT: &str = "project";

/// The Time Entry field linking its Task.
pub const TIME_ENTRY_TASK: &str = "task";

/// The Task owner field the before-save step derives, from the Project's owner.
pub const PROJECT_OWNER: &str = "owner";

/// The Task's own copy of the Project's owner (the `fetch_from` target; the
/// before-save step fills it, users never set it).
pub const TASK_PROJECT_OWNER: &str = "project_owner";

/// `task.assignee`: the user lookup filtered to enabled users only.
pub const TASK_ASSIGNEE: &str = "assignee";

/// The Task's confidential long-text field, in the `confidential` group.
pub const TASK_DETAILS: &str = "details";

/// The Task status field driving the blocked-reason display rule.
pub const TASK_STATUS: &str = "status";

/// The Task status value that makes `blocked_reason` relevant.
pub const TASK_STATUS_BLOCKED: &str = "Blocked";

/// The protection group only Project Managers may read.
pub const GROUP_CONFIDENTIAL: &str = "confidential";

/// The lowest hours a Time Entry may book (inclusive).
pub const HOURS_MIN: &str = "0.25";

/// The highest hours a Time Entry may book (inclusive).
pub const HOURS_MAX: &str = "24.00";

/// The hours bounds, exact decimals (ADR-12): (lowest, highest), inclusive.
pub const HOURS_BOUNDS: (&str, &str) = (HOURS_MIN, HOURS_MAX);
