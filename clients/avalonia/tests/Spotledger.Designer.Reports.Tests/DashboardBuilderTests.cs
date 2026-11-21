using Spotledger.Designer.Reports;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

public sealed class DashboardBuilderTests
{
    private static async Task<(FakeBuilderServer Server, DashboardBuilderViewModel Builder)> OpenAsync()
    {
        var server = new FakeBuilderServer();
        var builder = new DashboardBuilderViewModel(server, "dash-1");
        await builder.OpenAsync();
        return (server, builder);
    }

    [Fact]
    public async Task A_two_tile_dashboard_is_built_from_a_published_report_without_overlap()
    {
        var (server, builder) = await OpenAsync();
        Assert.Single(builder.Reports);

        Assert.True(await builder.AddTileAsync(TileKind.Number, "sales_report", "Sales", ["total"]));
        Assert.True(await builder.AddTileAsync(TileKind.Chart, "sales_report", "By territory", chart: new ChartSpec(ChartKind.Bar, "territory", ["total"])));

        Assert.Equal(2, builder.Draft!.Tiles.Count);
        Assert.False(builder.Draft.Tiles[0].Place.Overlaps(builder.Draft.Tiles[1].Place));
        Assert.Equal(
            [
                """{"op":"add_tile","tile":{"id":"number_1","title":"Sales","source":{"kind":"number","report":"sales_report","column":"total"},"place":{"row":1,"column":1,"width":3,"height":2},"uses_filters":true}}""",
                """{"op":"add_tile","tile":{"id":"chart_1","title":"By territory","source":{"kind":"chart","report":"sales_report","chart":{"kind":"bar","x":"territory","series":[{"column":"total"}]}},"place":{"row":1,"column":4,"width":6,"height":4},"uses_filters":true}}""",
            ],
            server.DashboardOps);
        Assert.Equal(2, builder.TilePreviews.Count);
        Assert.True(builder.PublishCommand.CanExecute(null));
    }

    [Fact]
    public async Task Moving_a_tile_by_keyboard_is_the_same_op_as_dragging_it()
    {
        var (server, builder) = await OpenAsync();
        await builder.AddTileAsync(TileKind.Number, "sales_report", "Sales", ["total"]);
        Assert.Equal("number_1", builder.SelectedTileId);

        Assert.True(await builder.MoveSelectedAsync(rows: 2, columns: 3));
        Assert.Equal(new Place(3, 4, 3, 2), builder.SelectedTile!.Place);

        Assert.True(await builder.MoveTileAsync("number_1", new Place(3, 4, 3, 2)));
        Assert.True(await builder.ResizeSelectedAsync(width: 1, height: 0));
        Assert.Equal(4, builder.SelectedTile!.Place.Width);
        Assert.Equal(
            """{"op":"move_tile","id":"number_1","place":{"row":3,"column":4,"width":3,"height":2}}""",
            server.DashboardOps[1]);
    }

    [Fact]
    public async Task The_server_decides_whether_a_place_is_free_and_its_refusal_is_shown()
    {
        var (_, builder) = await OpenAsync();
        await builder.AddTileAsync(TileKind.Number, "sales_report", "Sales", ["total"]);
        await builder.AddTileAsync(TileKind.Table, "sales_report", "All sales");
        var table = builder.Draft!.Tiles[1];

        Assert.False(await builder.MoveTileAsync(table.Id, new Place(1, 1, 12, 4)));

        Assert.Contains("on top of each other", builder.Status, StringComparison.Ordinal);
        Assert.Equal(table.Place, builder.Draft.Tiles[1].Place);
    }

    [Fact]
    public async Task A_full_dashboard_says_so_instead_of_guessing_a_place()
    {
        var (_, builder) = await OpenAsync();
        Assert.True(await builder.AddTileAsync(TileKind.Table, "sales_report", "One"));
        Assert.True(await builder.AddTileAsync(TileKind.Table, "sales_report", "Two"));

        Assert.False(await builder.AddTileAsync(TileKind.Table, "sales_report", "Three"));
        Assert.Contains("full", builder.Status, StringComparison.Ordinal);
        Assert.Equal(2, builder.Draft!.Tiles.Count);
    }

    [Fact]
    public async Task Tile_settings_shared_filters_refresh_and_sharing_are_ops()
    {
        var (server, builder) = await OpenAsync();
        await builder.AddTileAsync(TileKind.Number, "sales_report", "Sales", ["total"]);

        Assert.True(await builder.SetTileSettingsAsync("Revenue", usesFilters: false));
        Assert.True(await builder.AddSharedFilterAsync(new SharedFilter("year", "Year", ParamType.Whole, new FilterValue.Whole(2026))));
        Assert.True(await builder.SetRefreshAsync(new RefreshPolicy.Every(300)));
        Assert.True(await builder.ShareAsync(["Accounts Manager"]));

        Assert.Equal("Revenue", builder.Draft!.Tiles[0].Title);
        Assert.False(builder.Draft.Tiles[0].UsesFilters);
        Assert.Equal(
            [
                """{"op":"set_tile","id":"number_1","title":"Revenue","uses_filters":false}""",
                """{"op":"add_filter","filter":{"name":"year","label":"Year","ty":"int","default":{"kind":"int","value":2026}}}""",
                """{"op":"set_refresh","refresh":{"every":{"seconds":300}}}""",
                """{"op":"set_roles","roles":["Accounts Manager"]}""",
            ],
            server.DashboardOps.Skip(1));
    }

    [Fact]
    public async Task The_preview_runs_with_the_shared_filter_values_and_publish_waits_for_a_clean_validation()
    {
        var (_, builder) = await OpenAsync();
        builder.FilterValues["year"] = new FilterValue.Whole(2026);
        await builder.AddTileAsync(TileKind.Number, "other_report", "Elsewhere", ["total"]);

        Assert.Contains("1 filters", builder.TilePreviews[0].Summary, StringComparison.Ordinal);
        Assert.Contains(builder.Issues, i => i.Message.Contains("not published", StringComparison.Ordinal));
        Assert.False(builder.PublishCommand.CanExecute(null));

        await builder.RemoveSelectedAsync();
        await builder.AddTileAsync(TileKind.List, "sales_report", "Top", ["total", "territory"]);
        await Support.RunAsync(builder.PublishCommand);
        Assert.Equal("Published sales_board.", builder.PublishMessage);
    }
}
