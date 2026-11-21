//! The demo seed data (WP-DX-05.T1): exposure of `apps/sample/seed.toml` and the
//! counts the brief fixes.
//!
//! The file holds 2 projects, 6 tasks, 8 labels and 5 time entries, all drafts.
//! Its ids are placeholders an installer replaces with real record ids at import
//! (WP-PRD-01.T2's `spot install-app --seed`); links point at a row's `id` value
//! within the same file. Parsing and the metadata validation run in the detached
//! validation crate `tests/app-validate/` — the app crate itself carries no
//! TOML/serde dependency (G4 keeps the dependency list tight).

/// The DocType each top-level seed table loads into, in the file's own order.
pub const SEED_TABLES: [&str; 4] = ["labels", "projects", "tasks", "time_entries"];

/// The counts the brief fixes: labels, projects, tasks, time entries.
pub const SEED_COUNTS: (usize, usize, usize, usize) = (8, 2, 6, 5);

/// The number of Time Entry drafts in the seed.
#[must_use]
pub fn seed_time_entry_count() -> usize {
    SEED_COUNTS.3
}
