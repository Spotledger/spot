//! The dashboard draft and the dashboard definition it compiles to.
//!
//! A dashboard is a grid of tiles. Each tile shows one **published report**: a single number,
//! a chart, a table or a list of columns. The dashboard carries the filters every tile shares
//! (each one is a parameter the reports declare) and a refresh policy.
//!
//! # The definition format
//!
//! No dashboard definition format existed in the platform when this crate was written
//! (`spot-reports` defines reports only, WP-PLT-14), so [`DashboardDef`] is this crate's
//! **platform contract draft**: a versioned, TOML-serialisable document with a `schema`
//! number, the grid, the refresh policy, the shared filters and the tiles, which the client
//! renders and the native API serves. It is flagged under the PR's open questions so the
//! architecture owner can adopt it as a contract or replace it without touching a draft.

use serde::{Deserialize, Serialize};
use spot_reports::ReportRegistry;

use crate::chart::ChartSpec;
use crate::codes;
use crate::error::{Issue, OpError, is_blocked};
use crate::report::{DraftParamType, DraftStatus, FilterValue, is_name};
use crate::validate::dashboard_issues;

/// The schema version of [`DashboardDef`].
pub const DASHBOARD_SCHEMA_VERSION: u32 = 1;

/// The layout grid: how many columns a dashboard has and how many rows it starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grid {
    /// Columns, 1 to 24.
    pub columns: u8,
    /// Rows, at least 1.
    pub rows: u32,
}

impl Default for Grid {
    fn default() -> Self {
        Self {
            columns: 12,
            rows: 8,
        }
    }
}

/// Where one tile sits: a 1-based row and column, and how many cells it spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    /// The first row (1-based).
    pub row: u32,
    /// The first column (1-based).
    pub column: u8,
    /// How many columns it spans.
    pub width: u8,
    /// How many rows it spans.
    pub height: u8,
}

impl Place {
    /// A tile at `row`, `column`, spanning `width` × `height` cells.
    #[must_use]
    pub fn new(row: u32, column: u8, width: u8, height: u8) -> Self {
        Self {
            row,
            column,
            width,
            height,
        }
    }

    /// The last row the tile covers.
    #[must_use]
    pub fn last_row(&self) -> u32 {
        self.row
            .saturating_add(u32::from(self.height))
            .saturating_sub(1)
    }

    /// The last column the tile covers.
    #[must_use]
    pub fn last_column(&self) -> u8 {
        self.column.saturating_add(self.width).saturating_sub(1)
    }

    /// Whether two places share a cell.
    #[must_use]
    pub fn overlaps(&self, other: &Place) -> bool {
        self.row <= other.last_row()
            && other.row <= self.last_row()
            && self.column <= other.last_column()
            && other.column <= self.last_column()
    }
}

/// How often a dashboard refreshes its tiles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RefreshPolicy {
    /// Only when the viewer asks.
    Manual,
    /// Every time the viewer opens it.
    #[default]
    OnOpen,
    /// On a timer, in whole seconds.
    Every {
        /// The interval.
        seconds: u32,
    },
}

/// What a tile shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileKind {
    /// One large number from one report column.
    Number,
    /// A chart over a report.
    Chart,
    /// The report's columns as a table.
    Table,
    /// A short list of chosen columns.
    List,
}

impl TileKind {
    /// The name as written in a definition.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Chart => "chart",
            Self::Table => "table",
            Self::List => "list",
        }
    }
}

/// What a tile shows, and the report it shows it from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TileSource {
    /// One large number from a report column.
    Number {
        /// The report.
        report: String,
        /// The column shown.
        column: String,
    },
    /// A chart over the report's columns.
    Chart {
        /// The report.
        report: String,
        /// The chart.
        chart: ChartSpec,
    },
    /// The report as a table.
    Table {
        /// The report.
        report: String,
    },
    /// A short list of the report's columns.
    List {
        /// The report.
        report: String,
        /// The columns listed, in order.
        columns: Vec<String>,
    },
}

impl TileSource {
    /// What the tile shows.
    #[must_use]
    pub fn kind(&self) -> TileKind {
        match self {
            Self::Number { .. } => TileKind::Number,
            Self::Chart { .. } => TileKind::Chart,
            Self::Table { .. } => TileKind::Table,
            Self::List { .. } => TileKind::List,
        }
    }

    /// The report the tile shows.
    #[must_use]
    pub fn report(&self) -> &str {
        match self {
            Self::Number { report, .. }
            | Self::Chart { report, .. }
            | Self::Table { report }
            | Self::List { report, .. } => report,
        }
    }
}

/// One tile of a dashboard draft.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tile {
    /// The tile's id, unique in the dashboard.
    pub id: String,
    /// The heading a person reads.
    pub title: String,
    /// What it shows.
    pub source: TileSource,
    /// Where it sits.
    pub place: Place,
    /// Whether the dashboard's shared filters apply to it.
    #[serde(default = "yes")]
    pub uses_filters: bool,
}

fn yes() -> bool {
    true
}

impl Tile {
    /// A tile of any kind.
    #[must_use]
    pub fn new(id: &str, title: &str, source: TileSource, place: Place) -> Self {
        Self {
            id: id.to_owned(),
            title: title.to_owned(),
            source,
            place,
            uses_filters: true,
        }
    }

    /// A single-number tile.
    #[must_use]
    pub fn number(id: &str, title: &str, report: &str, column: &str, place: Place) -> Self {
        Self::new(
            id,
            title,
            TileSource::Number {
                report: report.to_owned(),
                column: column.to_owned(),
            },
            place,
        )
    }

    /// A chart tile.
    #[must_use]
    pub fn chart(id: &str, title: &str, report: &str, chart: ChartSpec, place: Place) -> Self {
        Self::new(
            id,
            title,
            TileSource::Chart {
                report: report.to_owned(),
                chart,
            },
            place,
        )
    }

    /// A table tile.
    #[must_use]
    pub fn table(id: &str, title: &str, report: &str, place: Place) -> Self {
        Self::new(
            id,
            title,
            TileSource::Table {
                report: report.to_owned(),
            },
            place,
        )
    }

    /// A list tile.
    #[must_use]
    pub fn list(id: &str, title: &str, report: &str, columns: &[&str], place: Place) -> Self {
        Self::new(
            id,
            title,
            TileSource::List {
                report: report.to_owned(),
                columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            },
            place,
        )
    }

    /// The same tile that ignores the dashboard's shared filters.
    #[must_use]
    pub fn without_shared_filters(mut self) -> Self {
        self.uses_filters = false;
        self
    }
}

/// A filter every tile that uses shared filters passes to its report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedFilter {
    /// The report parameter's name.
    pub name: String,
    /// The heading the viewer reads.
    pub label: String,
    /// The value's type.
    pub ty: DraftParamType,
    /// The value used when the viewer picks none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<FilterValue>,
}

impl SharedFilter {
    /// A shared filter named like the report parameter it binds.
    #[must_use]
    pub fn new(name: &str, label: &str, ty: DraftParamType) -> Self {
        Self {
            name: name.to_owned(),
            label: label.to_owned(),
            ty,
            default: None,
        }
    }
}

/// A dashboard draft: the tiles, the grid, the shared filters and the refresh policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardDraft {
    /// The draft's id.
    pub id: String,
    /// Who owns it.
    pub owner: String,
    /// The dashboard's storage name once published.
    pub name: String,
    /// The heading a person reads.
    pub label: String,
    /// Where the draft is in its life.
    #[serde(default)]
    pub status: DraftStatus,
    /// The layout grid.
    #[serde(default)]
    pub grid: Grid,
    /// How often it refreshes.
    #[serde(default)]
    pub refresh: RefreshPolicy,
    /// The filters every tile shares.
    #[serde(default)]
    pub filters: Vec<SharedFilter>,
    /// The tiles.
    #[serde(default)]
    pub tiles: Vec<Tile>,
    /// The roles that may open the dashboard once published.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl DashboardDraft {
    /// An empty dashboard.
    #[must_use]
    pub fn new(id: &str, owner: &str, name: &str, label: &str) -> Self {
        Self {
            id: id.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
            label: label.to_owned(),
            status: DraftStatus::Editing,
            grid: Grid::default(),
            refresh: RefreshPolicy::default(),
            filters: Vec::new(),
            tiles: Vec::new(),
            roles: Vec::new(),
        }
    }

    /// The tile called `id`, when the dashboard has one.
    #[must_use]
    pub fn tile(&self, id: &str) -> Option<&Tile> {
        self.tiles.iter().find(|t| t.id == id)
    }

    /// Adds a tile, refusing a duplicate id, a tile outside the grid or one that overlaps.
    ///
    /// # Errors
    ///
    /// An [`OpError`] naming what is wrong.
    pub fn add_tile(&mut self, tile: Tile) -> Result<(), OpError> {
        if self.tile(&tile.id).is_some() {
            return Err(OpError::Duplicate {
                name: tile.id.clone(),
            });
        }
        self.check_place(&tile.place, &tile.id)?;
        self.tiles.push(tile);
        Ok(())
    }

    /// Removes a tile, refusing an id the dashboard does not have.
    ///
    /// # Errors
    ///
    /// [`OpError::UnknownTile`] when no tile has that id.
    pub fn remove_tile(&mut self, id: &str) -> Result<(), OpError> {
        let before = self.tiles.len();
        self.tiles.retain(|t| t.id != id);
        if self.tiles.len() == before {
            return Err(OpError::UnknownTile {
                tile: id.to_owned(),
            });
        }
        Ok(())
    }

    /// Moves a tile, refusing an unknown id or a place outside the grid or on another tile.
    ///
    /// # Errors
    ///
    /// An [`OpError`] naming what is wrong.
    pub fn move_tile(&mut self, id: &str, place: Place) -> Result<(), OpError> {
        if self.tile(id).is_none() {
            return Err(OpError::UnknownTile {
                tile: id.to_owned(),
            });
        }
        self.check_place(&place, id)?;
        if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
            tile.place = place;
        }
        Ok(())
    }

    /// Adds a shared filter, refusing a duplicate name.
    ///
    /// # Errors
    ///
    /// [`OpError::Duplicate`] when a filter already has that name.
    pub fn add_filter(&mut self, filter: SharedFilter) -> Result<(), OpError> {
        if !is_name(&filter.name) {
            return Err(OpError::InvalidName { name: filter.name });
        }
        if self.filters.iter().any(|f| f.name == filter.name) {
            return Err(OpError::Duplicate { name: filter.name });
        }
        self.filters.push(filter);
        Ok(())
    }

    fn check_place(&self, place: &Place, tile: &str) -> Result<(), OpError> {
        if place.width == 0 || place.height == 0 || place.row == 0 || place.column == 0 {
            return Err(OpError::BadLayout {
                message: "A tile needs a row, a column and at least one cell.".to_owned(),
            });
        }
        if place.column > self.grid.columns || place.last_column() > self.grid.columns {
            return Err(OpError::BadLayout {
                message: format!(
                    "A tile does not fit: the dashboard is {} columns wide.",
                    self.grid.columns
                ),
            });
        }
        if place.last_row() > self.grid.rows {
            return Err(OpError::BadLayout {
                message: format!(
                    "A tile does not fit: the dashboard is {} rows tall.",
                    self.grid.rows
                ),
            });
        }
        for other in &self.tiles {
            if other.id != tile && other.place.overlaps(place) {
                return Err(OpError::BadLayout {
                    message: "Two tiles would sit on top of each other.".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// The published dashboard: the platform contract draft this crate defines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardDef {
    /// The schema version (`DASHBOARD_SCHEMA_VERSION`).
    pub schema: u32,
    /// The dashboard's storage name.
    pub name: String,
    /// The heading a person reads.
    pub label: String,
    /// The layout grid.
    pub grid: Grid,
    /// How often it refreshes.
    pub refresh: RefreshPolicy,
    /// The filters every tile shares.
    pub filters: Vec<SharedFilter>,
    /// The tiles, in placement order.
    pub tiles: Vec<TileDef>,
    /// The roles that may open it.
    pub roles: Vec<String>,
}

/// One tile of a published dashboard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileDef {
    /// The tile's id.
    pub id: String,
    /// The heading a person reads.
    pub title: String,
    /// What it shows.
    pub kind: TileKind,
    /// The report it shows.
    pub report: String,
    /// The column a number tile shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The columns a list tile shows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    /// The chart a chart tile draws.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartSpec>,
    /// Where it sits.
    pub place: Place,
    /// Whether the shared filters apply to it.
    pub uses_filters: bool,
}

impl DashboardDef {
    /// The tile called `id`, when the dashboard has one.
    #[must_use]
    pub fn tile(&self, id: &str) -> Option<&TileDef> {
        self.tiles.iter().find(|t| t.id == id)
    }

    /// The definition as TOML, the form a bundle and the client read.
    ///
    /// # Errors
    ///
    /// The serialiser's message (the definition is plain data, so this is a backstop).
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }
}

/// Compiles a dashboard draft against the reports the site has registered.
///
/// # Errors
///
/// Every issue the draft has: a structural problem, a tile naming a report or column the site
/// does not have, a chart its report cannot draw, or a shared filter a report does not
/// declare. Nothing is written.
pub fn compile_dashboard(
    draft: &DashboardDraft,
    reports: &ReportRegistry,
) -> Result<DashboardDef, Vec<Issue>> {
    let mut issues = dashboard_issues(draft);
    let mut tiles = Vec::with_capacity(draft.tiles.len());
    for (index, tile) in draft.tiles.iter().enumerate() {
        let at = format!("tiles[{index}]");
        let Some(registered) = reports.lookup(tile.source.report()) else {
            issues.push(Issue::at(
                codes::UNKNOWN_REPORT,
                &at,
                "This tile shows a report the site does not have.",
                "Choose a published report for the tile.",
            ));
            continue;
        };
        let columns = &registered.compiled.columns;
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let mut def = TileDef {
            id: tile.id.clone(),
            title: tile.title.clone(),
            kind: tile.source.kind(),
            report: tile.source.report().to_owned(),
            column: None,
            columns: Vec::new(),
            chart: None,
            place: tile.place,
            uses_filters: tile.uses_filters,
        };
        match &tile.source {
            TileSource::Number { column, .. } => {
                if !names.iter().any(|n| n == column) {
                    issues.push(Issue::at(
                        codes::UNKNOWN_COLUMN,
                        &at,
                        "The report does not have the column this number tile shows.",
                        "Choose one of the report's columns.",
                    ));
                }
                def.column = Some(column.clone());
            }
            TileSource::Chart { chart, .. } => {
                issues.extend(chart.validate(&names, &at));
                def.chart = Some(chart.clone());
            }
            TileSource::Table { .. } => {}
            TileSource::List { columns, .. } => {
                if columns.is_empty() {
                    issues.push(Issue::at(
                        codes::EMPTY,
                        &at,
                        "A list tile needs at least one column.",
                        "Choose the columns the list shows.",
                    ));
                }
                for column in columns {
                    if !names.iter().any(|n| n == column) {
                        issues.push(Issue::at(
                            codes::UNKNOWN_COLUMN,
                            &at,
                            "The report does not have a column this list tile shows.",
                            "Choose one of the report's columns.",
                        ));
                    }
                }
                def.columns = columns.clone();
            }
        }
        if tile.uses_filters {
            for filter in &draft.filters {
                if !registered
                    .compiled
                    .parameters
                    .iter()
                    .any(|p| p.name == filter.name)
                {
                    issues.push(Issue::at(
                        codes::UNKNOWN_PARAMETER,
                        &at,
                        "The report this tile shows does not offer one of the dashboard's filters.",
                        "Remove the filter from the tile, or add the same value to the report.",
                    ));
                }
            }
        }
        tiles.push(def);
    }
    if is_blocked(&issues) {
        return Err(issues);
    }
    Ok(DashboardDef {
        schema: DASHBOARD_SCHEMA_VERSION,
        name: draft.name.clone(),
        label: draft.label.clone(),
        grid: draft.grid,
        refresh: draft.refresh,
        filters: draft.filters.clone(),
        tiles,
        roles: draft.roles.clone(),
    })
}
