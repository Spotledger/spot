using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Designer.Reports;
using Spotledger.Designer.Reports.Views;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

/// <summary>The dashboard builder view, shown in a headless window and driven through its controls.</summary>
public sealed class DashboardBuilderViewTests
{
    private static Button GalleryButton(Shown<DashboardBuilderView, DashboardBuilderViewModel> shown, string label) =>
        shown.All<Button>().First(b => b.Content as string == label && b.Command == shown.Model.AddTileCommand);

    private static List<Border> Tiles(Shown<DashboardBuilderView, DashboardBuilderViewModel> shown) =>
        shown.Find<Canvas>("GridCanvas").Children.OfType<Border>().Where(b => b.Name != "DropHint").ToList();

    /// <summary>The strip at the top of a tile: the only part that starts a drag.</summary>
    private static Border Header(Border tile) => (Border)((StackPanel)tile.Child!).Children[0];

    /// <summary>The footprint a drag shows, or null when no drag is in progress.</summary>
    private static Border? Hint(Shown<DashboardBuilderView, DashboardBuilderViewModel> shown) =>
        shown.Find<Canvas>("GridCanvas").Children.OfType<Border>().FirstOrDefault(b => b.Name == "DropHint");

    private static Point InWindow(Control control, Point point, Window window) =>
        control.TranslatePoint(point, window) ?? throw new Xunit.Sdk.XunitException("The control is not on the window.");

    [AvaloniaFact]
    public async Task The_gallery_offers_every_tile_kind_including_the_chart()
    {
        var shown = await Views.DashboardAsync(new FakeBuilderServer());

        Assert.Equal(4, shown.Find<ItemsControl>("GalleryList").ItemCount);
        foreach (var label in new[] { "Number", "Chart", "Table", "List" })
        {
            Assert.NotNull(GalleryButton(shown, label));
        }

        foreach (var heading in new[] { "Add a tile", "Shared filters", "Refresh", "Share with", "Layout", "Preview", "Selected tile", "To fix or check" })
        {
            Assert.Contains(heading, shown.Texts());
        }
    }

    [AvaloniaFact]
    public async Task A_two_tile_dashboard_is_built_from_the_gallery_and_laid_out_on_the_grid()
    {
        var shown = await Views.DashboardAsync(new FakeBuilderServer());
        Assert.NotNull(shown.Model.SelectedReport);

        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));
        Clicks.On(GalleryButton(shown, "Chart"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 2));

        var draft = shown.Model.Draft!;
        Assert.Equal(TileKind.Number, draft.Tiles[0].Kind);
        Assert.Equal(ChartKind.Bar, draft.Tiles[1].Chart!.Kind);
        Assert.False(draft.Tiles[0].Place.Overlaps(draft.Tiles[1].Place));
        var tiles = Tiles(shown);
        Assert.Equal(12 * GridGeometry.CellWidth, shown.Find<Canvas>("GridCanvas").Width);
        Assert.Equal(Canvas.GetLeft(tiles[0]) + (3 * GridGeometry.CellWidth), Canvas.GetLeft(tiles[1]));
        Assert.Equal(2, shown.Find<ItemsControl>("PreviewList").ItemCount);
    }

    [AvaloniaFact]
    public async Task Alt_arrow_keys_move_the_focused_tile_and_alt_shift_arrows_resize_it()
    {
        var shown = await Views.DashboardAsync(new FakeBuilderServer());
        Clicks.On(GalleryButton(shown, "Table"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));
        await shown.Model.MoveTileAsync("table_1", new Place(1, 1, 4, 4));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));

        Tiles(shown)[0].Focus();
        Assert.Equal("table_1", shown.Model.SelectedTileId);

        shown.Window.KeyPressQwerty(PhysicalKey.ArrowRight, RawInputModifiers.Alt);
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Column == 2));
        // Focus stays on the tile after the grid is redrawn, so keyboard-only work can go on.
        shown.Window.KeyPressQwerty(PhysicalKey.ArrowDown, RawInputModifiers.Alt);
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Row == 2));
        shown.Window.KeyPressQwerty(PhysicalKey.ArrowRight, RawInputModifiers.Alt | RawInputModifiers.Shift);
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Width == 5));
        Assert.True(Tiles(shown)[0].IsFocused, "the tile should keep keyboard focus after it is redrawn");
    }

    [AvaloniaFact]
    public async Task Dragging_a_tile_on_the_grid_asks_the_server_to_move_it_and_a_refusal_snaps_it_back()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));

        var origin = shown.Model.SelectedTile!.Place;
        var tile = Tiles(shown)[0];
        var start = tile.TranslatePoint(new Point(5, 5), shown.Window)!.Value;

        shown.Window.MouseDown(start, MouseButton.Left);
        shown.Window.MouseMove(start + new Point(2 * GridGeometry.CellWidth, GridGeometry.CellHeight));
        shown.Window.MouseUp(start + new Point(2 * GridGeometry.CellWidth, GridGeometry.CellHeight), MouseButton.Left);
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Column == origin.Column + 2));
        Assert.Equal(origin.Row + 1, shown.Model.SelectedTile!.Place.Row);
        Assert.Contains(server.DashboardOps, op => op.Contains("\"op\":\"move_tile\"", StringComparison.Ordinal));

        // Dragging off the grid is refused by the server; the view shows why and draws the tile where it still is.
        var before = shown.Model.SelectedTile!.Place;
        var tile2 = Tiles(shown)[0];
        var from = tile2.TranslatePoint(new Point(5, 5), shown.Window)!.Value;
        var farAway = from + new Point(40 * GridGeometry.CellWidth, 0);
        shown.Window.MouseDown(from, MouseButton.Left);
        shown.Window.MouseMove(farAway);
        shown.Window.MouseUp(farAway, MouseButton.Left);
        Assert.True(await Clicks.StatusContainsAsync(shown.Find<TextBlock>("StatusText"), "does not fit"));
        Assert.Equal(before, shown.Model.SelectedTile!.Place);
        Assert.Equal((before.Column - 1) * GridGeometry.CellWidth, Canvas.GetLeft(Tiles(shown)[0]));
    }

    [AvaloniaFact]
    public async Task Tile_settings_shared_filters_refresh_and_sharing_reach_the_server()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));

        // Tile settings of the selected tile.
        Assert.Equal("Sales report", shown.Find<TextBox>("TileTitleBox").Text);
        shown.Find<TextBox>("TileTitleBox").Text = "Revenue";
        shown.Find<CheckBox>("TileFiltersBox").IsChecked = false;
        Clicks.On(shown.Find<Button>("ApplySettingsButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Title == "Revenue" && !shown.Model.SelectedTile.UsesFilters));

        // A shared filter, whose value feeds the preview.
        shown.Find<TextBox>("FilterName").Text = "territory";
        shown.Find<TextBox>("FilterLabel").Text = "Territory";
        Clicks.On(shown.Find<Button>("AddFilterButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.Draft!.Filters.Count == 1));
        Assert.True(await Pumping.WaitAsync(() => shown.Find<ItemsControl>("FilterValueList").ItemCount == 1));
        shown.Model.FilterInputs[0].Text = "north";
        Clicks.On(shown.Find<Button>("RefreshButton"));
        Assert.True(await Pumping.WaitAsync(() => server.LastDashboardFilters.ContainsKey("territory")));
        Assert.Equal(new FilterValue.Text("north"), server.LastDashboardFilters["territory"]);
        shown.Find<ListBox>("FilterList").SelectedIndex = 0;
        Clicks.On(shown.Find<Button>("RemoveFilterButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.Draft!.Filters.Count == 0));

        // Refresh policy and sharing.
        shown.Find<ComboBox>("RefreshBox").SelectedIndex = 2;
        shown.Find<NumericUpDown>("RefreshSeconds").Value = 30;
        Clicks.On(shown.Find<Button>("SetRefreshButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.Draft!.Refresh is RefreshPolicy.Every { Seconds: 30 }));
        shown.Find<TextBox>("RolesBox").Text = "Sales Manager";
        Clicks.On(shown.Find<Button>("ShareButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.Draft!.Roles.SequenceEqual(["Sales Manager"])));

        // Buttons move, resize and remove the selected tile.
        Clicks.On(shown.Find<Button>("WiderButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Width == 4));
        Clicks.On(shown.Find<Button>("DownButton"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Row == 2));
        Clicks.On(shown.Find<Button>("RemoveButton"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 0));
    }

    [AvaloniaFact]
    public async Task A_tile_preview_masks_a_column_the_viewer_may_not_read()
    {
        var server = new FakeBuilderServer { TilePreviewsCarryRows = true };
        var shown = await Views.DashboardAsync(server);
        Clicks.On(GalleryButton(shown, "Table"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.TilePreviews.Count == 1));

        var list = shown.Find<ItemsControl>("PreviewList");
        Assert.True(await Pumping.WaitAsync(() => list.GetVisualDescendantsOfText().Contains(RowsView.MaskText)));
        Assert.DoesNotContain("north", list.GetVisualDescendantsOfText());
        Assert.Contains("10.50", list.GetVisualDescendantsOfText());
    }

    [AvaloniaFact]
    public async Task A_dropped_connection_is_shown_in_the_status_line()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        server.Offline = true;

        Clicks.On(GalleryButton(shown, "Table"));
        Assert.True(await Clicks.StatusContainsAsync(shown.Find<TextBlock>("StatusText"), "That did not work"));
        Assert.Empty(shown.Model.Draft!.Tiles);

        server.Offline = false;
        Clicks.On(GalleryButton(shown, "Table"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.Draft!.Tiles.Count == 1));
        server.Offline = true;
        Clicks.On(shown.Find<Button>("PublishButton"));
        Assert.True(await Clicks.StatusContainsAsync(shown.Find<TextBlock>("StatusText"), "That did not work"));
    }

    [AvaloniaFact]
    public async Task The_dashboard_builder_renders_a_real_picture()
    {
        var shown = await Views.DashboardAsync(new FakeBuilderServer { TilePreviewsCarryRows = true });
        Clicks.On(GalleryButton(shown, "Number"));
        Clicks.On(GalleryButton(shown, "Chart"));
        Assert.True(await Pumping.WaitAsync(() => shown.Model.TilePreviews.Count == 2));

        Assert.True(Views.DistinctColours(shown.Window, "dashboard-builder") > 40, "the dashboard builder frame looks blank");
    }

    [AvaloniaFact]
    public async Task Dragging_a_tile_by_its_header_shows_a_green_footprint_and_drops_it_at_the_snapped_cell()
    {
        var shown = await Views.DashboardAsync(new FakeBuilderServer());
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));

        var origin = shown.Model.SelectedTile!.Place;
        var start = InWindow(Header(Tiles(shown)[0]), new Point(5, 5), shown.Window);
        var delta = new Point(2 * GridGeometry.CellWidth, GridGeometry.CellHeight);

        shown.Window.MouseDown(start, MouseButton.Left);
        shown.Window.MouseMove(start + delta);

        // The footprint sits on the cells the tile would take, in the free colour.
        var hint = Assert.IsType<Border>(Hint(shown));
        Assert.Equal(DashboardBuilderView.FreeHintTag, hint.Tag as string);
        Assert.Same(DashboardBuilderView.FreeCellBrush, hint.Background);
        Assert.Equal((origin.Column - 1 + 2) * GridGeometry.CellWidth, Canvas.GetLeft(hint));
        Assert.Equal((origin.Row - 1 + 1) * GridGeometry.CellHeight, Canvas.GetTop(hint));
        Assert.Equal((origin.Width * GridGeometry.CellWidth) - 2, hint.Width);
        Assert.Equal(shown.Model.DropTarget, new Place(origin.Row + 1, origin.Column + 2, origin.Width, origin.Height));

        shown.Window.MouseUp(start + delta, MouseButton.Left);

        Assert.True(await Pumping.WaitAsync(() => shown.Model.SelectedTile!.Place.Row == origin.Row + 1));
        Assert.Equal(origin.Column + 2, shown.Model.SelectedTile!.Place.Column);
        Assert.True(await Pumping.WaitAsync(() => Hint(shown) is null), "the footprint goes when the tile is dropped");
        Assert.Null(shown.Model.DropTarget);
        Assert.Equal((origin.Column + 1) * GridGeometry.CellWidth, Canvas.GetLeft(Tiles(shown)[0]));
    }

    [AvaloniaFact]
    public async Task A_drop_that_covers_another_tile_shows_red_and_changes_nothing()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 2));

        var first = shown.Model.Draft!.Tiles[0].Place;
        var secondTile = shown.Model.Draft!.Tiles[1];
        var second = secondTile.Place;
        var tile = Tiles(shown).Single(b => Equals(b.Tag, secondTile.Id));
        var start = InWindow(Header(tile), new Point(5, 5), shown.Window);
        var delta = new Point((first.Column - second.Column) * GridGeometry.CellWidth, 0);

        shown.Window.MouseDown(start, MouseButton.Left);
        shown.Window.MouseMove(start + delta);

        var hint = Assert.IsType<Border>(Hint(shown));
        Assert.Equal(DashboardBuilderView.BlockedHintTag, hint.Tag as string);
        Assert.Same(DashboardBuilderView.BlockedCellBrush, hint.Background);
        Assert.False(shown.Model.DropTargetIsFree);

        shown.Window.MouseUp(start + delta, MouseButton.Left);

        // The server refuses the place; the builder says why and the tile stays where it was.
        Assert.True(await Clicks.StatusContainsAsync(shown.Find<TextBlock>("StatusText"), "on top of each other"));
        Assert.Equal(second, shown.Model.Draft!.Tiles.Single(t => t.Id == secondTile.Id).Place);
        Assert.True(await Pumping.WaitAsync(() => Canvas.GetLeft(Tiles(shown).Single(b => Equals(b.Tag, secondTile.Id))) == (second.Column - 1) * GridGeometry.CellWidth));
        Assert.DoesNotContain(server.DashboardOps, op => op.Contains("\"move_tile\"", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public async Task Escape_abandons_a_drag_and_nothing_is_sent()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));

        var origin = shown.Model.SelectedTile!.Place;
        var start = InWindow(Header(Tiles(shown)[0]), new Point(5, 5), shown.Window);
        var delta = new Point(2 * GridGeometry.CellWidth, 0);

        shown.Window.MouseDown(start, MouseButton.Left);
        shown.Window.MouseMove(start + delta);
        Assert.NotNull(Hint(shown));

        shown.Window.KeyPressQwerty(PhysicalKey.Escape, RawInputModifiers.None);
        Assert.True(await Pumping.WaitAsync(() => Hint(shown) is null), "escape takes the footprint away");
        Assert.Null(shown.Model.DropTarget);

        // Releasing the button after escape does not send the move either.
        shown.Window.MouseUp(start + delta, MouseButton.Left);
        TestHost.Pump();

        Assert.Equal(origin, shown.Model.SelectedTile!.Place);
        Assert.Equal((origin.Column - 1) * GridGeometry.CellWidth, Canvas.GetLeft(Tiles(shown)[0]));
        Assert.DoesNotContain(server.DashboardOps, op => op.Contains("\"move_tile\"", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public async Task A_tile_moves_only_when_its_header_is_dragged()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        Clicks.On(GalleryButton(shown, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));

        var origin = shown.Model.SelectedTile!.Place;
        var body = InWindow(Tiles(shown)[0], new Point(5, 3 * GridGeometry.CellHeight / 2), shown.Window);
        var delta = new Point(2 * GridGeometry.CellWidth, 0);

        shown.Window.MouseDown(body, MouseButton.Left);
        shown.Window.MouseMove(body + delta);
        Assert.Null(Hint(shown));
        shown.Window.MouseUp(body + delta, MouseButton.Left);
        TestHost.Pump();

        Assert.Equal(origin, shown.Model.SelectedTile!.Place);
        Assert.DoesNotContain(server.DashboardOps, op => op.Contains("\"move_tile\"", StringComparison.Ordinal));
    }

    [AvaloniaFact]
    public async Task A_tile_kind_dragged_from_the_gallery_is_added_where_it_is_dropped()
    {
        var server = new FakeBuilderServer();
        var shown = await Views.DashboardAsync(server);
        var canvas = shown.Find<Canvas>("GridCanvas");

        var start = InWindow(GalleryButton(shown, "Number"), new Point(8, 8), shown.Window);
        var drop = InWindow(canvas, new Point((3 * GridGeometry.CellWidth) + 20, (2 * GridGeometry.CellHeight) + 15), shown.Window);

        shown.Window.MouseDown(start, MouseButton.Left);
        shown.Window.MouseMove(drop);

        var hint = Assert.IsType<Border>(Hint(shown));
        Assert.Equal(DashboardBuilderView.FreeHintTag, hint.Tag as string);
        Assert.Equal(3 * GridGeometry.CellWidth, Canvas.GetLeft(hint));
        Assert.Equal(2 * GridGeometry.CellHeight, Canvas.GetTop(hint));

        shown.Window.MouseUp(drop, MouseButton.Left);

        Assert.True(await Pumping.WaitAsync(() => Tiles(shown).Count == 1));
        var tile = Assert.Single(shown.Model.Draft!.Tiles);
        Assert.Equal(new Place(3, 4, 3, 2), tile.Place);
        Assert.True(await Pumping.WaitAsync(() => Hint(shown) is null));

        // One op: the drag added the tile and the gallery button's own click did not also add one.
        Assert.Single(server.DashboardOps);
        Assert.Contains("\"add_tile\"", server.DashboardOps[0], StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public async Task A_drag_and_the_same_keyboard_nudge_send_the_same_op()
    {
        var dragServer = new FakeBuilderServer();
        var drag = await Views.DashboardAsync(dragServer);
        Clicks.On(GalleryButton(drag, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(drag).Count == 1));
        var start = InWindow(Header(Tiles(drag)[0]), new Point(5, 5), drag.Window);
        var down = new Point(0, GridGeometry.CellHeight);
        drag.Window.MouseDown(start, MouseButton.Left);
        drag.Window.MouseMove(start + down);
        drag.Window.MouseUp(start + down, MouseButton.Left);
        Assert.True(await Pumping.WaitAsync(() => drag.Model.SelectedTile!.Place.Row == 2));

        var keyServer = new FakeBuilderServer();
        var keyboard = await Views.DashboardAsync(keyServer);
        Clicks.On(GalleryButton(keyboard, "Number"));
        Assert.True(await Pumping.WaitAsync(() => Tiles(keyboard).Count == 1));
        Tiles(keyboard)[0].Focus();
        keyboard.Window.KeyPressQwerty(PhysicalKey.ArrowDown, RawInputModifiers.Alt);
        Assert.True(await Pumping.WaitAsync(() => keyboard.Model.SelectedTile!.Place.Row == 2));

        // The pointer route is not a second implementation: both send one move_tile op with the
        // same id and the same place, in the same order after the add.
        var dragged = dragServer.DashboardOps.Where(op => op.Contains("\"move_tile\"", StringComparison.Ordinal)).ToList();
        var nudged = keyServer.DashboardOps.Where(op => op.Contains("\"move_tile\"", StringComparison.Ordinal)).ToList();
        Assert.Single(dragged);
        Assert.Equal(nudged, dragged);
        Assert.Equal(new Place(2, 1, 3, 2), drag.Model.SelectedTile!.Place);
        Assert.Equal(drag.Model.SelectedTile.Place, keyboard.Model.SelectedTile!.Place);
        Assert.Equal(
            dragServer.DashboardOps.Select(op => op.Contains("\"add_tile\"", StringComparison.Ordinal)),
            keyServer.DashboardOps.Select(op => op.Contains("\"add_tile\"", StringComparison.Ordinal)));
    }
}
