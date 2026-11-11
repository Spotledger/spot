//! The span-aware TOML parser for `spot-app.toml` (ADR-17).
//!
//! It reads a manifest into the typed model or reports **every** problem at once as
//! [`ManifestError`]s with stable codes and `line, column` positions. The shared rules —
//! owning steps, once-per hook point, budgets, unique schedule names, ordered and unique
//! patch ids, the override `by` rule — live in [`crate::validate`] and run on both
//! file-parsed and code-built manifests, so they can never drift apart.

use std::ops::Range;
use std::time::Duration;

use semver::{Version, VersionReq};
use toml_edit::{ImDocument, Item, Table, TableLike, TomlError};

use spot_app_api::StepName;
use spot_app_api::prelude::Name;

use crate::cron::Cron;
use crate::error::ManifestError;
use crate::error::line_col;
use crate::model::{
    AppDependency, AppManifest, AppMeta, AppPermissionName, AssetSub, BootSub, Capabilities,
    DocTypeAccess, EVENT_BUDGET_DEFAULT_MS, Endpoint, EventSub, FilePath, FixtureSub, FunctionSub,
    HttpVerb, MethodSub, OverrideDecl, PatchSub, PermissionSub, RoutePath, RouteSub,
    SCHEDULE_BUDGET_DEFAULT_MS, ScheduleSub, SeamDecl, SeamKind, SessionSub,
};

/// The tables a manifest may carry, in the order FORMAT.md documents them.
const ROOTS: [&str; 15] = [
    "app",
    "methods",
    "permissions",
    "functions",
    "fixtures",
    "patches",
    "routes",
    "assets",
    "events",
    "session_events",
    "boot",
    "schedules",
    "overrides",
    "seams",
    "capabilities",
];

/// Parses a manifest and validates it. Returns **all** problems at once, each with its
/// position in `source`. `origin` names the file in the message, such as `spot-app.toml`.
pub fn parse(source: &str, origin: &str) -> Result<AppManifest, Vec<ManifestError>> {
    // `ImDocument` keeps every item's byte span; `DocumentMut` throws the spans away when
    // it parses, and errors must be able to point at a line and column.
    let doc = match ImDocument::parse(source) {
        Ok(doc) => doc,
        Err(err) => return Err(vec![syntax_error(&err, source, origin)]),
    };
    let root = doc.as_table();
    let mut w = Walker {
        source,
        errs: Vec::new(),
        out: Out::default(),
        spans: crate::validate::Spans::default(),
    };
    for (key, item) in root.iter() {
        if !ROOTS.contains(&key) {
            // A root table's item has no span of its own; point at its `[header]`.
            let span = item.span().or_else(|| find_header(w.source, key));
            w.err(span, crate::error::UNKNOWN_KEY, format!(
                "the manifest has no table `{key}` (see FORMAT.md for the tables an app declares)"));
        }
    }
    app_table(&mut w, root, origin);
    list(&mut w, root, "events", event_row);
    list(&mut w, root, "session_events", session_row);
    list(&mut w, root, "boot", boot_row);
    list(&mut w, root, "schedules", schedule_row);
    list(&mut w, root, "overrides", override_row);
    list(&mut w, root, "seams", seam_row);
    list(&mut w, root, "methods", method_row);
    list(&mut w, root, "permissions", permission_row);
    list(&mut w, root, "functions", function_row);
    list(&mut w, root, "fixtures", fixture_row);
    list(&mut w, root, "patches", patch_row);
    list(&mut w, root, "routes", route_row);
    list(&mut w, root, "assets", asset_row);
    capabilities_table(&mut w, root);
    // Validate what was successfully read even when other rows failed: a broken file must
    // still surface its cross-row problems in the same pass, so the author fixes everything
    // in one go.
    let Some(app) = w.out.app.take() else {
        return Err(w.errs);
    };
    let manifest = AppManifest {
        app,
        events: w.out.events,
        session_events: w.out.session_events,
        boot: w.out.boot,
        schedules: w.out.schedules,
        overrides: w.out.overrides,
        seams: w.out.seams,
        methods: w.out.methods,
        functions: w.out.functions,
        fixtures: w.out.fixtures,
        patches: w.out.patches,
        routes: w.out.routes,
        assets: w.out.assets,
        capabilities: w.out.capabilities,
        permissions: w.out.permissions,
    };
    let mut errs = std::mem::take(&mut w.errs);
    errs.extend(crate::validate::check_rules(&manifest, Some(&w.spans)));
    if errs.is_empty() {
        Ok(manifest)
    } else {
        Err(errs)
    }
}

/// The one syntax error a broken file produces.
fn syntax_error(err: &TomlError, source: &str, origin: &str) -> ManifestError {
    let built = ManifestError::new(
        crate::error::INVALID_TOML,
        format!("{origin}: the TOML does not parse: {}", err.message()),
    );
    match err.span() {
        Some(range) => {
            let (line, col) = line_col(source, range.start);
            built.at(line, col)
        }
        None => built,
    }
}

/// The manifest parts the parser collects before assembling an [`AppManifest`].
#[derive(Default)]
struct Out {
    app: Option<AppMeta>,
    events: Vec<EventSub>,
    session_events: Vec<SessionSub>,
    boot: Vec<BootSub>,
    schedules: Vec<ScheduleSub>,
    overrides: Vec<OverrideDecl>,
    seams: Vec<SeamDecl>,
    methods: Vec<MethodSub>,
    functions: Vec<FunctionSub>,
    fixtures: Vec<FixtureSub>,
    patches: Vec<PatchSub>,
    routes: Vec<RouteSub>,
    assets: Vec<AssetSub>,
    capabilities: Capabilities,
    permissions: Vec<PermissionSub>,
}

/// The parser state: errors, finished rows and their source positions.
struct Walker<'src> {
    source: &'src str,
    errs: Vec<ManifestError>,
    out: Out,
    /// Field positions, handed to the shared checks in [`crate::validate`].
    spans: crate::validate::Spans,
}

impl<'src> Walker<'src> {
    /// Pushes an error, attaching `span`'s start as the position.
    fn err(&mut self, span: Option<Range<usize>>, code: &'static str, msg: impl Into<String>) {
        let err = ManifestError::new(code, msg);
        self.errs.push(match span {
            Some(range) if range.start <= self.source.len() => {
                let (line, col) = line_col(self.source, range.start);
                err.at(line, col)
            }
            _ => err,
        });
    }

    /// Pushes an already-built error, attaching the position only if it has none.
    fn push(&mut self, err: ManifestError, span: Option<Range<usize>>) {
        self.errs.push(if err.line.is_none() {
            match span {
                Some(range) if range.start <= self.source.len() => {
                    let (line, col) = line_col(self.source, range.start);
                    err.at(line, col)
                }
                _ => err,
            }
        } else {
            err
        });
    }

    /// Reports keys a section does not know. Callers list their allowed keys, so unknown
    /// keys stand out no matter how the manifest grows.
    fn known(&mut self, t: &dyn TableLike, kind: &str, allowed: &[&str]) {
        for (key, item) in t.iter() {
            if !allowed.contains(&key) {
                self.err(
                    item.span(),
                    crate::error::UNKNOWN_KEY,
                    format!("{kind} has no key `{key}`"),
                );
            }
        }
    }

    /// A required string key; empty strings are not accepted.
    fn need<'t>(
        &mut self,
        t: &'t dyn TableLike,
        key: &'static str,
        kind: &str,
        fallback: Option<Range<usize>>,
    ) -> Option<(&'t str, Option<Range<usize>>)> {
        match t.get(key) {
            None => {
                self.err(
                    fallback,
                    crate::error::MISSING_KEY,
                    format!("{kind} needs a `{key}`"),
                );
                None
            }
            Some(item) => match item.as_str() {
                Some("") => {
                    self.err(
                        item.span(),
                        crate::error::MISSING_KEY,
                        format!("{kind} `{key}` must not be empty"),
                    );
                    None
                }
                Some(raw) => Some((raw, item.span())),
                None => {
                    self.err(
                        item.span(),
                        crate::error::INVALID_TYPE,
                        format!(
                            "{kind} `{key}` must be a string, but it is a {}",
                            item.type_name()
                        ),
                    );
                    None
                }
            },
        }
    }

    /// A required string key that may be empty, for callers that judge emptiness themselves
    /// (an empty permission `title` is a `manifest.permission_title`, not a missing key).
    fn required_str<'t>(
        &mut self,
        t: &'t dyn TableLike,
        key: &'static str,
        kind: &str,
        fallback: Option<Range<usize>>,
    ) -> Option<(&'t str, Option<Range<usize>>)> {
        match t.get(key) {
            None => {
                self.err(
                    fallback,
                    crate::error::MISSING_KEY,
                    format!("{kind} needs a `{key}`"),
                );
                None
            }
            Some(item) => match item.as_str() {
                Some(raw) => Some((raw, item.span())),
                None => {
                    self.err(
                        item.span(),
                        crate::error::INVALID_TYPE,
                        format!(
                            "{kind} `{key}` must be a string, but it is a {}",
                            item.type_name()
                        ),
                    );
                    None
                }
            },
        }
    }

    /// An optional string key.
    fn opt_str<'t>(
        &mut self,
        t: &'t dyn TableLike,
        key: &'static str,
        kind: &str,
    ) -> Option<(&'t str, Option<Range<usize>>)> {
        match t.get(key) {
            None => None,
            Some(item) => match item.as_str() {
                Some(raw) => Some((raw, item.span())),
                None => {
                    self.err(
                        item.span(),
                        crate::error::INVALID_TYPE,
                        format!(
                            "{kind} `{key}` must be a string, but it is a {}",
                            item.type_name()
                        ),
                    );
                    None
                }
            },
        }
    }

    /// Reads an optional boolean key; the row is dropped when the value is invalid.
    fn boolean(
        &mut self,
        t: &dyn TableLike,
        key: &'static str,
        kind: &str,
        default: bool,
    ) -> Option<bool> {
        if t.get(key).is_none() {
            return Some(default);
        }
        self.opt_bool(t, key, kind)
    }

    /// An optional boolean key; `None` means present but invalid (already reported).
    fn opt_bool(&mut self, t: &dyn TableLike, key: &'static str, kind: &str) -> Option<bool> {
        match t.get(key) {
            None => None,
            Some(item) => match item.as_bool() {
                Some(value) => Some(value),
                None => {
                    self.err(
                        item.span(),
                        crate::error::INVALID_TYPE,
                        format!("{kind} `{key}` must be a boolean"),
                    );
                    None
                }
            },
        }
    }

    /// An optional millisecond budget with its default; reports negative and non-integer
    /// values as errors.
    fn opt_budget(
        &mut self,
        t: &dyn TableLike,
        kind: &str,
        default_ms: u64,
    ) -> Option<(Duration, Option<(u32, u32)>)> {
        match t.get("budget_ms") {
            None => Some((Duration::from_millis(default_ms), None)),
            Some(item) => match item.as_integer() {
                Some(ms) if ms >= 0 => Some((
                    Duration::from_millis(ms as u64),
                    pos(self.source, item.span()),
                )),
                Some(_) => {
                    self.err(
                        item.span(),
                        crate::error::BUDGET_OUT_OF_RANGE,
                        format!("{kind} `budget_ms` cannot be negative"),
                    );
                    None
                }
                None => {
                    self.err(
                        item.span(),
                        crate::error::INVALID_TYPE,
                        format!("{kind} `budget_ms` must be a whole number of milliseconds"),
                    );
                    None
                }
            },
        }
    }

    /// Reads a step name, such as `erp_selling:quote_totals`.
    fn step_of(&mut self, raw: &str, span: Option<Range<usize>>, kind: &str) -> Option<StepName> {
        match StepName::new(raw) {
            Ok(step) => Some(step),
            Err(_) => {
                self.err(
                    span,
                    crate::error::INVALID_STEP,
                    format!("{kind} takes a step as `<app>:<step>`, but it is {raw:?}"),
                );
                None
            }
        }
    }

    /// Reads a storage-style name.
    fn name_of(&mut self, raw: &str, span: Option<Range<usize>>, what: &str) -> Option<Name> {
        match Name::new(raw) {
            Ok(name) => Some(name),
            Err(_) => {
                self.push(crate::event::invalid_name(what, raw), span);
                None
            }
        }
    }

    /// Reads a semver version.
    fn version_of(&mut self, raw: &str, span: Option<Range<usize>>, what: &str) -> Option<Version> {
        match crate::event::parse_version(raw, what) {
            Ok(version) => Some(version),
            Err(err) => {
                self.push(err, span);
                None
            }
        }
    }

    /// Reads a semver requirement.
    fn req_of(&mut self, raw: &str, span: Option<Range<usize>>, what: &str) -> Option<VersionReq> {
        match crate::event::parse_version_req(raw, what) {
            Ok(req) => Some(req),
            Err(err) => {
                self.push(err, span);
                None
            }
        }
    }
}

/// A byte offset as a 1-based line and column, when a span exists.
fn pos(source: &str, span: Option<Range<usize>>) -> Option<(u32, u32)> {
    span.map(|range| line_col(source, range.start))
}

/// Finds the `[key]` or `[[key]]` header of a root table in the source text. `toml_edit`
/// gives a root table's item no span of its own, so unknown-root-table errors use this.
/// Quoted or dotted headers simply end up without a position.
fn find_header(source: &str, key: &str) -> Option<std::ops::Range<usize>> {
    let head = format!("[{key}]");
    let array = format!("[[{key}]]");
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with(head.as_str()) || trimmed.starts_with(array.as_str()) {
            let start = offset + (line.len() - trimmed.len());
            return Some(start..start + key.len() + 2);
        }
        offset += line.len();
    }
    None
}

// [part B]
/// Walks `[app]`, the one table every manifest must have.
fn app_table(w: &mut Walker<'_>, root: &Table, origin: &str) {
    let Some(item) = root.get("app") else {
        w.err(
            None,
            crate::error::MISSING_KEY,
            format!("{origin}: the manifest has no `[app]` table"),
        );
        return;
    };
    let Some(t) = item.as_table() else {
        w.err(
            item.span(),
            crate::error::INVALID_TYPE,
            "[app] must be a table",
        );
        return;
    };
    w.known(
        t,
        "[app]",
        &["name", "version", "title", "requires_host", "depends"],
    );
    let fallback = t.span();
    let Some((name_raw, name_span)) = w.need(t, "name", "[app]", fallback.clone()) else {
        return;
    };
    let Some(name) = w.name_of(name_raw, name_span, "app name") else {
        return;
    };
    let Some((version_raw, version_span)) = w.need(t, "version", "[app]", fallback) else {
        return;
    };
    let Some(version) = w.version_of(version_raw, version_span, "[app] `version`") else {
        return;
    };
    let title = w
        .opt_str(t, "title", "[app]")
        .map_or("", |(raw, _)| raw)
        .to_owned();
    let requires_host = if t.get("requires_host").is_some() {
        let Some((req_raw, req_span)) = w.opt_str(t, "requires_host", "[app]") else {
            return;
        };
        let Some(req) = w.req_of(req_raw, req_span, "[app] `requires_host`") else {
            return;
        };
        Some(req)
    } else {
        None
    };
    let depends = match t.get("depends") {
        Some(item) => depends_rows(w, item),
        None => Vec::new(),
    };
    w.out.app = Some(AppMeta {
        name,
        version,
        title,
        requires_host,
        depends,
    });
}

/// Reads `depends`, written as `[[app.depends]]` rows or a list of inline tables.
fn depends_rows(w: &mut Walker<'_>, item: &Item) -> Vec<AppDependency> {
    const BAD: &str = "[app] `depends` must be a list of tables, written as `[[app.depends]]` rows or `{ app = ..., version = ... }`";
    let mut deps = Vec::new();
    match item {
        Item::ArrayOfTables(tables) => {
            for row in tables.iter() {
                if let Some(dep) = dep_row(w, row, row.span()) {
                    deps.push(dep);
                }
            }
        }
        Item::Value(value) => match value.as_array() {
            Some(array) => {
                for entry in array.iter() {
                    let Some(row) = entry.as_inline_table() else {
                        w.err(
                            entry.span(),
                            crate::error::INVALID_TYPE,
                            "each app.depends entry must be a table like `{ app = ..., version = ... }`",
                        );
                        continue;
                    };
                    if let Some(dep) = dep_row(w, row, entry.span()) {
                        deps.push(dep);
                    }
                }
            }
            None => match value.as_inline_table() {
                Some(row) => {
                    if let Some(dep) = dep_row(w, row, value.span()) {
                        deps.push(dep);
                    }
                }
                None => {
                    w.err(value.span(), crate::error::INVALID_TYPE, BAD);
                }
            },
        },
        _ => {
            w.err(item.span(), crate::error::INVALID_TYPE, BAD);
        }
    }
    deps
}

/// Reads one dependency row.
fn dep_row(
    w: &mut Walker<'_>,
    row: &dyn TableLike,
    fallback: Option<Range<usize>>,
) -> Option<AppDependency> {
    w.known(row, "[app] depends", &["app", "version"]);
    let (dep_raw, dep_span) = w.need(row, "app", "[app] depends", fallback.clone())?;
    let dep = w.name_of(dep_raw, dep_span, "[app] depends `app`")?;
    let (req_raw, req_span) = w.need(row, "version", "[app] depends", fallback)?;
    let req = w.req_of(req_raw, req_span, "[app] depends `version`")?;
    Some(AppDependency {
        app: dep,
        version: req,
    })
}

/// Walks `[capabilities]`, defaulting to the least access.
fn capabilities_table(w: &mut Walker<'_>, root: &Table) {
    let Some(item) = root.get("capabilities") else {
        return;
    };
    let Some(t) = item.as_table() else {
        w.err(
            item.span(),
            crate::error::INVALID_TYPE,
            "[capabilities] must be a table",
        );
        return;
    };
    w.known(
        t,
        "[capabilities]",
        &["network", "email", "files", "doctypes"],
    );
    let mut caps = Capabilities::default();
    if t.get("network").is_some() {
        let Some(network) = endpoint_list(w, t) else {
            return;
        };
        caps.network = network;
    }
    if let Some(flag) = w.opt_bool(t, "email", "[capabilities]") {
        caps.email = flag;
    }
    if let Some(files) = t.get("files") {
        match files.as_str() {
            Some(raw) => match crate::event::parse_file_access(raw) {
                Ok(access) => caps.files = access,
                Err(err) => {
                    w.push(err, files.span());
                    return;
                }
            },
            None => {
                w.err(
                    files.span(),
                    crate::error::INVALID_TYPE,
                    "capabilities.files must be one of none, read, read_write",
                );
                return;
            }
        }
    }
    if let Some(doctypes) = t.get("doctypes") {
        let Some(side) = doctypes.as_table_like() else {
            w.err(
                doctypes.span(),
                crate::error::INVALID_TYPE,
                "capabilities.doctypes must be a table with `read` and `write` lists of DocType names",
            );
            return;
        };
        w.known(side, "capabilities.doctypes", &["read", "write"]);
        let Some(read) = name_list(w, side, "read") else {
            return;
        };
        let Some(write) = name_list(w, side, "write") else {
            return;
        };
        caps.doctypes = DocTypeAccess { read, write };
    }
    w.out.capabilities = caps;
}

/// The endpoints `[capabilities] network` allows, when the key is present.
fn endpoint_list(w: &mut Walker<'_>, t: &dyn TableLike) -> Option<Vec<Endpoint>> {
    const BAD: &str = "capabilities.network must be a list of \"host:port\" endpoint strings";
    let item = t.get("network")?;
    let Some(array) = item.as_array() else {
        w.err(item.span(), crate::error::INVALID_TYPE, BAD);
        return None;
    };
    let mut endpoints = Vec::new();
    for entry in array.iter() {
        let Some(raw) = entry.as_str() else {
            w.err(
                entry.span(),
                crate::error::INVALID_TYPE,
                "each capabilities.network entry must be a \"host:port\" string",
            );
            return None;
        };
        match crate::event::parse_endpoint(raw) {
            Ok(endpoint) => endpoints.push(endpoint),
            Err(err) => {
                w.push(err, entry.span());
                return None;
            }
        }
    }
    Some(endpoints)
}

/// A list of DocType labels, such as `Sales Invoice`.
fn name_list(w: &mut Walker<'_>, t: &dyn TableLike, key: &'static str) -> Option<Vec<String>> {
    let Some(item) = t.get(key) else {
        return Some(Vec::new());
    };
    let Some(array) = item.as_array() else {
        w.err(
            item.span(),
            crate::error::INVALID_TYPE,
            format!("capabilities.doctypes `{key}` must be a list of DocType labels"),
        );
        return None;
    };
    let mut labels = Vec::new();
    for entry in array.iter() {
        let Some(raw) = entry.as_str() else {
            w.err(
                entry.span(),
                crate::error::INVALID_TYPE,
                format!("each capabilities.doctypes `{key}` entry must be a string DocType label"),
            );
            return None;
        };
        if let Err(err) = crate::model::check_doctype(raw) {
            w.push(err, entry.span());
            return None;
        }
        labels.push(raw.to_owned());
    }
    Some(labels)
}

/// Reads a `[[section]]`, reporting each row through `row`.
fn list(w: &mut Walker<'_>, root: &Table, key: &'static str, row: fn(&mut Walker<'_>, &Table)) {
    let Some(item) = root.get(key) else { return };
    match item.as_array_of_tables() {
        Some(rows) => {
            for table in rows.iter() {
                row(w, table);
            }
        }
        None => w.err(
            item.span(),
            crate::error::INVALID_TYPE,
            format!("the `{key}` section must be a list, written as `[[{key}]]`"),
        ),
    }
}

// [part C]
/// Reads one `[[events]]` row.
fn event_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[events]]";
    w.known(
        t,
        KIND,
        &["doctype", "event", "step", "priority", "budget_ms"],
    );
    let fallback = t.span();
    let Some((doctype, _)) = w.need(t, "doctype", KIND, fallback.clone()) else {
        return;
    };
    let Some((event_raw, event_span)) = w.need(t, "event", KIND, fallback.clone()) else {
        return;
    };
    let event = match crate::event::parse_hook_point(event_raw) {
        Ok(event) => event,
        Err(err) => {
            w.push(err, event_span);
            return;
        }
    };
    let Some((step_raw, step_span)) = w.need(t, "step", KIND, fallback.clone()) else {
        return;
    };
    let Some(step) = w.step_of(step_raw, step_span.clone(), KIND) else {
        return;
    };
    let priority = match t.get("priority") {
        None => 0,
        Some(item) => match item.as_integer() {
            Some(value) => match i32::try_from(value) {
                Ok(priority) => priority,
                Err(_) => {
                    w.err(
                        item.span(),
                        crate::error::INVALID_TYPE,
                        "`priority` must be a signed 32-bit integer",
                    );
                    return;
                }
            },
            None => {
                w.err(
                    item.span(),
                    crate::error::INVALID_TYPE,
                    format!("{KIND} `priority` must be an integer"),
                );
                return;
            }
        },
    };
    let Some((budget, budget_pos)) = w.opt_budget(t, KIND, EVENT_BUDGET_DEFAULT_MS) else {
        return;
    };
    w.spans.events.push(crate::validate::Pos {
        anchor: pos(w.source, step_span),
        budget: budget_pos,
    });
    w.out.events.push(EventSub {
        doctype: doctype.to_owned(),
        event,
        step,
        priority,
        budget,
    });
}

/// Reads one `[[session_events]]` row.
fn session_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[session_events]]";
    w.known(t, KIND, &["event", "step", "budget_ms"]);
    let fallback = t.span();
    let Some((event_raw, event_span)) = w.need(t, "event", KIND, fallback.clone()) else {
        return;
    };
    let event = match crate::event::parse_session_event(event_raw) {
        Ok(event) => event,
        Err(err) => {
            w.push(err, event_span);
            return;
        }
    };
    let Some((step_raw, step_span)) = w.need(t, "step", KIND, fallback.clone()) else {
        return;
    };
    let Some(step) = w.step_of(step_raw, step_span.clone(), KIND) else {
        return;
    };
    let Some((budget, budget_pos)) = w.opt_budget(t, KIND, EVENT_BUDGET_DEFAULT_MS) else {
        return;
    };
    w.spans.session.push(crate::validate::Pos {
        anchor: pos(w.source, step_span),
        budget: budget_pos,
    });
    w.out.session_events.push(SessionSub {
        event,
        step,
        budget,
    });
}

/// Reads one `[[boot]]` row. Boot runs once per deployment, so there is no default budget.
fn boot_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[boot]]";
    w.known(t, KIND, &["step", "budget_ms"]);
    let fallback = t.span();
    let Some((step_raw, step_span)) = w.need(t, "step", KIND, fallback) else {
        return;
    };
    let Some(step) = w.step_of(step_raw, step_span.clone(), KIND) else {
        return;
    };
    let budget = match t.get("budget_ms") {
        None => None,
        Some(item) => match item.as_integer() {
            Some(ms) if ms >= 0 => Some(Duration::from_millis(ms as u64)),
            Some(_) => {
                w.err(
                    item.span(),
                    crate::error::BUDGET_OUT_OF_RANGE,
                    "[[boot]] `budget_ms` cannot be negative",
                );
                return;
            }
            None => {
                w.err(
                    item.span(),
                    crate::error::INVALID_TYPE,
                    "[[boot]] `budget_ms` must be a whole number of milliseconds",
                );
                return;
            }
        },
    };
    w.spans.boot.push(crate::validate::Pos {
        anchor: pos(w.source, step_span),
        budget: None,
    });
    w.out.boot.push(BootSub { step, budget });
}

/// Reads one `[[schedules]]` row.
fn schedule_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[schedules]]";
    w.known(t, KIND, &["name", "step", "cron", "queue", "budget_ms"]);
    let fallback = t.span();
    let Some((name_raw, _)) = w.need(t, "name", KIND, fallback.clone()) else {
        return;
    };
    let Some((step_raw, step_span)) = w.need(t, "step", KIND, fallback.clone()) else {
        return;
    };
    let Some(step) = w.step_of(step_raw, step_span.clone(), KIND) else {
        return;
    };
    let Some((cron_raw, cron_span)) = w.need(t, "cron", KIND, fallback.clone()) else {
        return;
    };
    let cron = match Cron::parse(cron_raw) {
        Ok(cron) => cron,
        Err(err) => {
            w.push(err, cron_span);
            return;
        }
    };
    let queue = match w.opt_str(t, "queue", KIND) {
        Some((queue_raw, queue_span)) => match crate::event::parse_queue(queue_raw) {
            Ok(queue) => queue,
            Err(err) => {
                w.push(err, queue_span);
                return;
            }
        },
        None => crate::model::Queue::Default,
    };
    let Some((budget, budget_pos)) = w.opt_budget(t, KIND, SCHEDULE_BUDGET_DEFAULT_MS) else {
        return;
    };
    w.spans.schedules.push(crate::validate::Pos {
        anchor: pos(w.source, step_span),
        budget: budget_pos,
    });
    w.out.schedules.push(ScheduleSub {
        name: name_raw.to_owned(),
        cron,
        step,
        queue,
        budget,
    });
}

/// Reads one `[[overrides]]` row (ADR-08): how this app changes one step, usually another
/// app's.
fn override_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[overrides]]";
    w.known(t, KIND, &["target", "kind", "by", "reason"]);
    let fallback = t.span();
    let Some((target_raw, target_span)) = w.need(t, "target", KIND, fallback.clone()) else {
        return;
    };
    let Some(target) = w.step_of(target_raw, target_span.clone(), KIND) else {
        return;
    };
    let Some((kind_raw, kind_span)) = w.need(t, "kind", KIND, fallback.clone()) else {
        return;
    };
    let kind = match crate::event::parse_override_kind(kind_raw) {
        Ok(kind) => kind,
        Err(err) => {
            w.push(err, kind_span);
            return;
        }
    };
    let by = if t.get("by").is_some() {
        match w
            .opt_str(t, "by", KIND)
            .and_then(|(raw, span)| w.step_of(raw, span.clone(), KIND).map(|step| (step, span)))
        {
            Some((step, _)) => Some(step),
            None => return,
        }
    } else {
        None
    };
    let reason = w
        .opt_str(t, "reason", KIND)
        .map_or("", |(raw, _)| raw)
        .to_owned();
    w.spans.overrides.push(crate::validate::Pos {
        anchor: pos(w.source, target_span),
        budget: None,
    });
    w.out.overrides.push(OverrideDecl {
        target,
        kind,
        by,
        reason,
    });
}
/// Reads one `[[seams]]` row (ADR-08): a seam this app provides or replaces.
fn seam_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[seams]]";
    w.known(t, KIND, &["provide", "replace"]);
    match (t.get("provide"), t.get("replace")) {
        (Some(side), None) => seam_side(w, side, SeamKind::Provide),
        (None, Some(side)) => seam_side(w, side, SeamKind::Replace),
        (None, None) => {
            w.err(
                t.span(),
                crate::error::INVALID_SEAM,
                "[[seams]] needs a `provide` or a `replace` table",
            );
        }
        (Some(_), Some(_)) => {
            w.err(
                t.span(),
                crate::error::INVALID_SEAM,
                "[[seams]] cannot both `provide` and `replace`: it is one or the other",
            );
        }
    }
}

/// Reads the `provide` or `replace` side of one `[[seams]]` row.
fn seam_side(w: &mut Walker<'_>, side: &Item, kind: SeamKind) {
    let what = match kind {
        SeamKind::Provide => "[[seams]] `provide`",
        SeamKind::Replace => "[[seams]] `replace`",
    };
    let side_span = side.span();
    let Some(t) = side.as_table_like() else {
        w.err(
            side_span,
            crate::error::INVALID_TYPE,
            format!("{what} must be an inline table"),
        );
        return;
    };
    w.known(
        t,
        what,
        match kind {
            SeamKind::Provide => &["name", "version"],
            SeamKind::Replace => &["name", "version", "reason"],
        },
    );
    let Some((name_raw, name_span)) = w.need(t, "name", what, side_span.clone()) else {
        return;
    };
    if !seam_name_ok(name_raw) {
        w.err(
            name_span,
            crate::error::INVALID_SEAM,
            format!(
                "seam name {name_raw:?}: use a dotted name of storage-style parts or \
                 `app:seam`, like `tax.calculate` or `erp_selling:report_render`"
            ),
        );
        return;
    }
    let version = match t.get("version") {
        None => {
            w.err(
                side_span,
                crate::error::MISSING_KEY,
                format!("{what} needs a `version`"),
            );
            return;
        }
        Some(item) => match item.as_integer() {
            Some(value) if value > 0 && u32::try_from(value).is_ok() => value as u32,
            Some(_) => {
                w.err(
                    item.span(),
                    crate::error::INVALID_SEAM,
                    "seam versions are whole numbers starting at 1",
                );
                return;
            }
            None => {
                w.err(
                    item.span(),
                    crate::error::INVALID_TYPE,
                    "`version` must be a whole number",
                );
                return;
            }
        },
    };
    let reason = if kind == SeamKind::Replace {
        let Some((reason, _)) = w.need(t, "reason", what, side_span.clone()) else {
            return;
        };
        Some(reason.to_owned())
    } else {
        None
    };
    w.out.seams.push(SeamDecl {
        name: name_raw.to_owned(),
        version,
        kind,
        reason,
    });
}

/// A seam name: dotted (`tax.calculate`) or `app:seam`, each part a storage-style name.
fn seam_name_ok(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    match raw.split_once(':') {
        Some((app, seam)) => Name::new(app).is_ok() && Name::new(seam).is_ok(),
        None => raw.split('.').all(|part| Name::new(part).is_ok()),
    }
}

/// Reads one `[[methods]]` row (ADR-16).
fn method_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[methods]]";
    w.known(t, KIND, &["doctype", "name", "mutates"]);
    let fallback = t.span();
    let Some((doctype, _)) = w.need(t, "doctype", KIND, fallback.clone()) else {
        return;
    };
    let Some((name_raw, name_span)) = w.need(t, "name", KIND, fallback.clone()) else {
        return;
    };
    let Some(name) = w.name_of(name_raw, name_span, "method name") else {
        return;
    };
    let Some(mutates) = w.boolean(t, "mutates", KIND, false) else {
        return;
    };
    w.out.methods.push(MethodSub {
        doctype: doctype.to_owned(),
        name,
        mutates,
    });
}

/// Reads one `[[functions]]` row (ADR-16).
fn function_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[functions]]";
    w.known(t, KIND, &["module", "name", "http", "guest"]);
    let fallback = t.span();
    let Some((module_raw, module_span)) = w.need(t, "module", KIND, fallback.clone()) else {
        return;
    };
    let Some(module) = w.name_of(module_raw, module_span, "module name") else {
        return;
    };
    let Some((name_raw, name_span)) = w.need(t, "name", KIND, fallback.clone()) else {
        return;
    };
    let Some(name) = w.name_of(name_raw, name_span, "function name") else {
        return;
    };
    let http = if t.get("http").is_some() {
        match verb_list(w, t) {
            Some(http) => Some(http),
            None => return,
        }
    } else {
        None
    };
    let Some(guest) = w.boolean(t, "guest", KIND, false) else {
        return;
    };
    w.out.functions.push(FunctionSub {
        module,
        name,
        http,
        guest,
    });
}

/// The HTTP verbs a whitelisted function answers, when the row states them.
fn verb_list(w: &mut Walker<'_>, t: &dyn TableLike) -> Option<Vec<HttpVerb>> {
    let item = t.get("http")?;
    let Some(array) = item.as_array() else {
        w.err(
            item.span(),
            crate::error::INVALID_METHOD,
            "[[functions]] `http` must be a list of verbs, like [\"GET\", \"POST\"]",
        );
        return None;
    };
    let mut verbs = Vec::new();
    for entry in array.iter() {
        let Some(raw) = entry.as_str() else {
            w.err(
                entry.span(),
                crate::error::INVALID_METHOD,
                "each `http` entry must be a verb string, like \"GET\"",
            );
            return None;
        };
        match crate::event::parse_verb(raw) {
            Ok(verb) => {
                if !verbs.contains(&verb) {
                    verbs.push(verb);
                }
            }
            Err(err) => {
                w.push(err, entry.span());
                return None;
            }
        }
    }
    Some(verbs)
}

/// Reads one `[[fixtures]]` row.
fn fixture_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[fixtures]]";
    w.known(t, KIND, &["doctype", "file", "step"]);
    let fallback = t.span();
    let Some((doctype, _)) = w.need(t, "doctype", KIND, fallback.clone()) else {
        return;
    };
    let Some((file_raw, file_span)) = w.need(t, "file", KIND, fallback.clone()) else {
        return;
    };
    let file = match FilePath::parse(file_raw) {
        Ok(file) => file,
        Err(err) => {
            w.push(err, file_span);
            return;
        }
    };
    let mut step_pos = None;
    let step = if t.get("step").is_some() {
        let Some((step_raw, step_span)) = w.need(t, "step", KIND, fallback.clone()) else {
            return;
        };
        let Some(step) = w.step_of(step_raw, step_span.clone(), KIND) else {
            return;
        };
        step_pos = pos(w.source, step_span);
        Some(step)
    } else {
        None
    };
    w.spans.fixtures.push(crate::validate::Pos {
        anchor: step_pos.or_else(|| pos(w.source, file_span)),
        budget: None,
    });
    w.out.fixtures.push(FixtureSub {
        doctype: doctype.to_owned(),
        file,
        step,
    });
}

/// Reads one `[[patches]]` row (ADR-22).
fn patch_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[patches]]";
    w.known(
        t,
        KIND,
        &["id", "file", "intent", "reversible", "idempotent"],
    );
    let fallback = t.span();
    let Some((id, _)) = w.need(t, "id", KIND, fallback.clone()) else {
        return;
    };
    let Some((file_raw, file_span)) = w.need(t, "file", KIND, fallback.clone()) else {
        return;
    };
    let file = match FilePath::parse(file_raw) {
        Ok(file) => file,
        Err(err) => {
            w.push(err, file_span);
            return;
        }
    };
    let Some((intent_raw, intent_span)) = w.need(t, "intent", KIND, fallback.clone()) else {
        return;
    };
    let intent = match crate::event::parse_intent(intent_raw) {
        Ok(intent) => intent,
        Err(err) => {
            w.push(err, intent_span);
            return;
        }
    };
    let Some(reversible) = w.boolean(t, "reversible", KIND, false) else {
        return;
    };
    let Some(idempotent) = w.boolean(t, "idempotent", KIND, false) else {
        return;
    };
    w.spans.patches.push(crate::validate::Pos {
        anchor: pos(w.source, t.get("id").and_then(|item| item.span())),
        budget: None,
    });
    w.out.patches.push(PatchSub {
        id: id.to_owned(),
        file,
        intent,
        reversible,
        idempotent,
    });
}

/// Reads one `[[routes]]` row.
fn route_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[routes]]";
    w.known(t, KIND, &["path", "step"]);
    let fallback = t.span();
    let Some((path_raw, path_span)) = w.need(t, "path", KIND, fallback.clone()) else {
        return;
    };
    let path = match RoutePath::parse(path_raw) {
        Ok(path) => path,
        Err(err) => {
            w.push(err, path_span);
            return;
        }
    };
    let Some((step_raw, step_span)) = w.need(t, "step", KIND, fallback.clone()) else {
        return;
    };
    let Some(step) = w.step_of(step_raw, step_span.clone(), KIND) else {
        return;
    };
    w.spans.routes.push(crate::validate::Pos {
        anchor: pos(w.source, step_span),
        budget: None,
    });
    w.out.routes.push(RouteSub { path, step });
}

/// Reads one `[[assets]]` row.
fn asset_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[assets]]";
    w.known(t, KIND, &["path"]);
    let fallback = t.span();
    let Some((path_raw, path_span)) = w.need(t, "path", KIND, fallback) else {
        return;
    };
    match FilePath::parse(path_raw) {
        Ok(path) => w.out.assets.push(AssetSub { path }),
        Err(err) => w.push(err, path_span),
    }
}

/// Reads one `[[permissions]]` row (WP-PLT-03.T4): a named permission the app declares.
///
/// The name is validated here, so the model only ever holds valid names; the title's length
/// and emptiness are judged by [`crate::validate`], which also sees code-built manifests.
fn permission_row(w: &mut Walker<'_>, t: &Table) {
    const KIND: &str = "[[permissions]]";
    w.known(t, KIND, &["name", "title", "description"]);
    let fallback = t.span();
    let Some((name_raw, name_span)) = w.required_str(t, "name", KIND, fallback.clone()) else {
        return;
    };
    let name = match AppPermissionName::parse(name_raw) {
        Ok(name) => name,
        Err(err) => {
            w.push(err, name_span);
            return;
        }
    };
    let Some((title, title_span)) = w.required_str(t, "title", KIND, fallback) else {
        return;
    };
    let description = w
        .opt_str(t, "description", KIND)
        .map(|(raw, _)| raw.to_owned());
    w.spans.permissions.push(crate::validate::PermPos {
        name: pos(w.source, name_span),
        title: pos(w.source, title_span),
    });
    w.out.permissions.push(PermissionSub {
        name,
        title: title.to_owned(),
        description,
    });
}
