using Spotledger.Client.Grid;
using Avalonia.Headless.XUnit;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Column geometry: px/star widths, frozen hit-testing, sort cycle, resize and
/// auto-fit behaviour.</summary>
public sealed class ColumnAndSortTests
{
    private static GridColumnModel[] MakeColumns(params GridColumnModel[] defs) => defs;

    private static GridColumnModel Frozen(double px) => new()
    {
        Header = "F",
        Width = GridColumnWidth.Pixels(px),
        IsFrozen = true,
    };

    private static GridColumnModel Star(double weight = 1) => new()
    {
        Header = "S",
        Width = GridColumnWidth.Star(weight),
    };

    private static GridColumnModel Pixels(double px, string header = "P") => new()
    {
        Header = header,
        Width = GridColumnWidth.Pixels(px),
    };

    private static SpotGrid MakeGrid(GridColumnModel[] columns, out InMemoryGridSource source)
    {
        source = new InMemoryGridSource(GridFixture.MakeRows(50, columns.Length));
        return GridFixtures.Make(source)
            .With(source, [.. columns], width: 600, height: 300)
            .Build();
    }

    [AvaloniaFact]
    public void StarColumnsSplitTheViewportBudget()
    {
        var grid = new SpotGrid
        {
            Columns = MakeColumns(Pixels(80), Star(), Star()),
            Source = new InMemoryGridSource(GridFixture.MakeRows(5, 3)),
        };
        TestHost.Show(grid, width: 600, height: 300);
        TestHost.Pump();
        grid.SetViewport(600, 200);
        grid.ComputeColumnLayout(600);

        Assert.Equal(80, grid.ColumnWidths![0], 1);
        Assert.Equal(260, grid.ColumnWidths[1], 1);
        Assert.Equal(260, grid.ColumnWidths[2], 1);
    }

    [AvaloniaFact]
    public void StarWeightTwoGetsTwiceTheShare()
    {
        var grid = new SpotGrid
        {
            Columns = MakeColumns(Star(2), Star(1)),
            Source = new InMemoryGridSource(GridFixture.MakeRows(5, 2)),
        };
        TestHost.Show(grid, width: 600, height: 300);
        grid.SetViewport(600, 200);
        grid.ComputeColumnLayout(600);

        Assert.Equal(400, grid.ColumnWidths![0], 1);
        Assert.Equal(200, grid.ColumnWidths[1], 1);
    }

    [AvaloniaFact]
    public void MinWidthFloorsACollapsedColumn()
    {
        var narrow = Pixels(10);
        narrow.MinWidth = 40;
        var grid = new SpotGrid
        {
            Columns = MakeColumns(narrow, Star()),
            Source = new InMemoryGridSource(GridFixture.MakeRows(5, 2)),
        };
        TestHost.Show(grid, width: 600, height: 300);
        grid.SetViewport(600, 200);
        grid.ComputeColumnLayout(600);

        Assert.Equal(40, grid.ColumnWidths![0], 1);
    }

    [AvaloniaFact]
    public void FrozenRunHonoursOnlyALeadingContiguousSet()
    {
        var middle = Pixels(60);
        middle.IsFrozen = true; // NOT leading: must be ignored
        var columns = MakeColumns(Frozen(80), Star(), middle);
        var grid = MakeGrid(columns, out _);

        Assert.Equal(1, grid.FrozenColumnCount);
    }

    [AvaloniaFact]
    public void HitTestMapsFrozenAndScrolledColumns()
    {
        var columns = MakeColumns(Frozen(80), Star(), Pixels(100));
        var grid = MakeGrid(columns, out _);
        grid.SetViewport(600, 200);
        grid.ComputeColumnLayout(600);

        // y below the header (>=40) to land in a body cell.
        Assert.Equal(0, grid.ColumnAtX(10, out _)); // frozen zone
        Assert.Equal(1, grid.ColumnAtX(90, out _)); // first scrolled column
        Assert.Equal(2, grid.ColumnAtX(90 + ColumnWidthFor(grid, 1) + 30, out _));
    }

    private static double ColumnWidthFor(SpotGrid grid, int index) => grid.ColumnWidths![index];

    [AvaloniaFact]
    public void HitTestFindsRowsFromScrollOffset()
    {
        var columns = MakeColumns(Star());
        var grid = MakeGrid(columns, out _);
        grid.SetViewport(600, 200);
        grid.ComputeColumnLayout(600);

        var hit = grid.HitTest(100, 65); // header 40 + 25 into row zone
        Assert.Equal(GridArea.Cell, hit.Area);
        Assert.Equal(0, hit.RowIndex);

        grid.ScrollToRow(3);
        hit = grid.HitTest(100, 65);
        Assert.Equal(3, hit.RowIndex);
    }

    [AvaloniaFact]
    public void HitTestDetectsHeaderResizeEdge()
    {
        var columns = MakeColumns(Pixels(100), Star());
        var grid = MakeGrid(columns, out _);
        grid.SetViewport(600, 200);
        grid.ComputeColumnLayout(600);

        var hit = grid.HitTest(97, 20); // trailing 5px grip of column 0, header band
        Assert.Equal(GridArea.HeaderEdge, hit.Area);
        Assert.Equal(0, hit.ColumnIndex);
    }

    [AvaloniaFact]
    public void HeaderClickCyclesNoneAscDescNoneAndPushesToSource()
    {
        var columns = MakeColumns(Pixels(100, "num"));
        var source = new InMemoryGridSource(
            GridFixture.MakeRows(20, 1),
            row => row.Cells[0].Text);
        var grid = new SpotGrid { Columns = columns, Source = source };
        TestHost.Show(grid);
        TestHost.Pump();
        TestHost.Pump(); // window completion

        var sorts = new List<(int Column, GridSortDirection Direction)>();
        grid.Sorted += (_, e) => sorts.Add((e.ColumnIndex, e.Direction));

        grid.CycleSort(0);
        Assert.Equal(GridSortDirection.Ascending, sorts[0].Direction);

        grid.CycleSort(0);
        Assert.Equal(GridSortDirection.Descending, sorts[1].Direction);

        grid.CycleSort(0);
        Assert.Equal(GridSortDirection.None, sorts[2].Direction);

        Assert.Equal(3, source.SortCalls.Count);
        Assert.Equal((0, GridSortDirection.Ascending), source.SortCalls[0]);
        Assert.Equal((0, GridSortDirection.Descending), source.SortCalls[1]);
        Assert.Equal((0, GridSortDirection.None), source.SortCalls[2]);
    }

    [AvaloniaFact]
    public void NonSortableColumnIgnoresHeaderClick()
    {
        var quiet = Pixels(100, "quiet");
        quiet.IsSortable = false;
        var source = new InMemoryGridSource(GridFixture.MakeRows(5, 1), row => row.Cells[0].Text);
        var grid = new SpotGrid { Columns = MakeColumns(quiet), Source = source };
        TestHost.Show(grid);
        TestHost.Pump();

        grid.CycleSort(0);
        Assert.Empty(source.SortCalls);
        Assert.Equal(-1, grid.SortState.Column);
        Assert.Equal(GridSortDirection.None, grid.SortState.Direction);
    }

    [AvaloniaFact]
    public void AutoFitSetsWidthToWidestVisibleCell()
    {
        var narrow = Pixels(30, "W");
        narrow.MinWidth = 20;
        var source = new InMemoryGridSource(GridFixture.MakeRows(10, 1));
        var grid = new SpotGrid { Columns = MakeColumns(narrow), Source = source };
        TestHost.Show(grid);
        TestHost.Pump();

        grid.AutoFitColumn(0);
        var width = grid.ColumnWidths![0];
        Assert.True(width > 20, $"auto-fit must widen past the min; got {width}");
        var textWidth = SpotGrid.MeasureCellTextWidth("row 9"); // widest "row N" of 10 rows
        Assert.Equal(textWidth + (grid.CellPadding * 2) + 4, width, 2);
    }

    [AvaloniaFact]
    public void KeyboardReorderMovesColumnAndRaises()
    {
        var columns = MakeColumns(Pixels(60, "A"), Pixels(60, "B"), Pixels(60, "Z"));
        var source = new InMemoryGridSource(GridFixture.MakeRows(5, 3));
        var grid = new SpotGrid { Columns = columns, Source = source };
        TestHost.Show(grid);
        TestHost.Pump();

        var moved = new List<(int From, int To)>();
        grid.ColumnReordered += (_, e) => moved.Add((e.FromIndex, e.ToIndex));

        grid.ReorderColumnKeyboard(0, 1);
        Assert.Equal((0, 1), Assert.Single(moved));
        Assert.Equal("B", columns[0].Header);
        Assert.Equal("A", columns[1].Header);
    }
}