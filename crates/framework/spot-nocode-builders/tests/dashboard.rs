//! Dashboards: the four tile kinds compile to the dashboard definition, the grid is checked,
//! and the tiles bind the report's own columns and parameters.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_nocode_builders::{
    ChartSeries, ChartSpec, Compare, DashboardDraft, DraftColumn, DraftFilter, DraftParamType,
    DraftParameter, FilterTarget, Grid, Place, RefreshPolicy, ReportDraft, SharedFilter, Tile,
    TileSource, compile_dashboard,
};
use spot_reports::ReportRegistry;
use support::{World, world};

/// The report a dashboard tile shows.
fn sales_draft() -> ReportDraft {
    let mut draft = ReportDraft::new("draft-3", "gina", "sales", "Sales", "journal");
    draft.columns = vec![
        DraftColumn::new("date").with_label("Month"),
        DraftColumn::new("debit")
            .with_label("Debited")
            .with_format("money"),
    ];
    draft.parameters = vec![DraftParameter::date("from", "From")];
    draft.filters = vec![DraftFilter::new(
        "date",
        Compare::Ge,
        FilterTarget::parameter("from"),
    )];
    draft.limit = Some(1_000);
    draft.roles = vec!["reader".to_owned(), "analyst".to_owned()];
    draft
}

fn registry_with(world: &World) -> ReportRegistry {
    let set = world.meta_set();
    let plan = support::compile(world, &sales_draft()).expect("the tile's report compiles");
    let mut registry = ReportRegistry::new();
    registry
        .register(
            "nocode",
            Arc::new(plan.definition),
            &support::report_compiler(world, &set),
        )
        .expect("registers");
    registry
}

fn dashboard() -> DashboardDraft {
    let mut draft = DashboardDraft::new("dash-1", "gina", "sales_overview", "Sales overview");
    draft.grid = Grid {
        columns: 12,
        rows: 6,
    };
    draft.refresh = RefreshPolicy::Every { seconds: 300 };
    draft.filters = vec![SharedFilter::new("from", "From", DraftParamType::Date)];
    draft.roles = vec!["reader".to_owned(), "analyst".to_owned()];
    draft
        .add_tile(Tile::number(
            "total",
            "Total debited",
            "sales",
            "debit",
            Place::new(1, 1, 3, 2),
        ))
        .expect("fits");
    draft
        .add_tile(Tile::chart(
            "by_month",
            "By month",
            "sales",
            ChartSpec::bar("date", vec![ChartSeries::new("debit")]),
            Place::new(1, 4, 9, 3),
        ))
        .expect("fits");
    draft
        .add_tile(Tile::table(
            "all",
            "All rows",
            "sales",
            Place::new(4, 1, 6, 2),
        ))
        .expect("fits");
    draft
        .add_tile(Tile::list(
            "recent",
            "Recent",
            "sales",
            &["date", "debit"],
            Place::new(4, 7, 6, 2),
        ))
        .expect("fits");
    draft
}

#[tokio::test]
async fn a_dashboard_compiles_to_the_definition() {
    let world = world().await;
    let registry = registry_with(&world);
    let draft = dashboard();
    let def = compile_dashboard(&draft, &registry).expect("the dashboard compiles");

    assert_eq!(def.schema, 1);
    assert_eq!(def.name, "sales_overview");
    assert_eq!(def.tiles.len(), 4);
    assert_eq!(def.refresh, RefreshPolicy::Every { seconds: 300 });
    assert_eq!(def.filters.len(), 1);
    let number = def.tile("total").expect("the number tile");
    assert_eq!(number.column.as_deref(), Some("debit"));
    let chart = def.tile("by_month").expect("the chart tile");
    assert!(chart.chart.is_some());
    let list = def.tile("recent").expect("the list tile");
    assert_eq!(list.columns, vec!["date".to_owned(), "debit".to_owned()]);

    let text = def.to_toml().expect("the definition serialises");
    assert!(text.contains("schema = 1"), "{text}");
    assert!(text.contains("by_month"), "{text}");
}

#[tokio::test]
async fn a_tile_must_fit_inside_the_grid_and_not_overlap() {
    let mut draft = dashboard();
    let outside = Tile::table("outside", "Outside", "sales", Place::new(1, 10, 4, 1));
    let err = draft.add_tile(outside).expect_err("does not fit");
    assert_eq!(err.code(), "nocode_builders.bad_layout");

    let overlapping = Tile::table("overlap", "Overlap", "sales", Place::new(1, 1, 2, 2));
    let err = draft.add_tile(overlapping).expect_err("overlaps");
    assert_eq!(err.code(), "nocode_builders.bad_layout");

    assert!(draft.remove_tile("missing").is_err());
    draft
        .move_tile("recent", Place::new(6, 1, 12, 1))
        .expect("moves");
}

#[tokio::test]
async fn a_tile_naming_a_report_or_column_the_site_lacks_is_refused() {
    let world = world().await;
    let registry = registry_with(&world);

    let mut draft = dashboard();
    draft.tiles.push(Tile::table(
        "ghost",
        "Ghost",
        "no_such",
        Place::new(6, 1, 3, 1),
    ));
    let issues = compile_dashboard(&draft, &registry).expect_err("the report is missing");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.unknown_report"),
        "{issues:?}"
    );

    let mut draft = dashboard();
    draft.tiles.push(Tile::number(
        "wrong",
        "Wrong",
        "sales",
        "memo",
        Place::new(6, 1, 3, 1),
    ));
    let issues = compile_dashboard(&draft, &registry).expect_err("the column is missing");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.unknown_column"),
        "{issues:?}"
    );

    let mut draft = dashboard();
    draft
        .filters
        .push(SharedFilter::new("region", "Region", DraftParamType::Text));
    let issues = compile_dashboard(&draft, &registry).expect_err("the report has no region");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.unknown_parameter"),
        "{issues:?}"
    );
}

#[tokio::test]
async fn a_zero_interval_refresh_is_refused() {
    let world = world().await;
    let registry = registry_with(&world);
    let mut draft = dashboard();
    draft.refresh = RefreshPolicy::Every { seconds: 0 };
    let issues = compile_dashboard(&draft, &registry).expect_err("refreshing every zero seconds");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.bad_refresh"),
        "{issues:?}"
    );
}

#[tokio::test]
async fn a_chart_the_report_cannot_draw_is_refused() {
    let world = world().await;
    let registry = registry_with(&world);
    let mut draft = dashboard();
    draft.tiles.retain(|tile| tile.id != "by_month");
    draft.tiles.push(Tile::chart(
        "broken",
        "Broken",
        "sales",
        ChartSpec {
            kind: spot_nocode_builders::ChartKind::Pie,
            x: Some("date".to_owned()),
            series: vec![ChartSeries::new("date"), ChartSeries::new("debit")],
            title: None,
        },
        Place::new(1, 4, 9, 3),
    ));
    let issues = compile_dashboard(&draft, &registry).expect_err("the pie draws two columns");
    assert!(
        issues.iter().any(|i| i.code == "nocode_builders.bad_chart"),
        "{issues:?}"
    );
}

#[tokio::test]
async fn a_tile_can_opt_out_of_the_shared_filters() {
    let world = world().await;
    let registry = registry_with(&world);
    let mut draft = dashboard();
    draft.filters = vec![SharedFilter::new("region", "Region", DraftParamType::Text)];
    // Every filter must bind to each report the tiles show; opting out keeps the tile usable.
    for tile in &mut draft.tiles {
        tile.uses_filters = false;
    }
    let def = compile_dashboard(&draft, &registry).expect("tiles opt out");
    assert!(def.tiles.iter().all(|tile| !tile.uses_filters));
    assert!(matches!(draft.tiles[0].source, TileSource::Number { .. }));
}
