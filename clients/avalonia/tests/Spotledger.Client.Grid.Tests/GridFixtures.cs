using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Spotledger.Client.Grid;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Shared fixture source for the grid tests: how a SpotGrid is built and shown
/// in the headless host. The per-file MakeGrid helpers delegate here — the builder lists
/// each file's specifics (columns, source, selection mode, window size, pump count) so
/// two fixtures never disagree about ordering.</summary>
public static class GridFixtures
{
    /// <summary>Builder for one grid under test; call <see cref="Build"/> from it.</summary>
    public sealed class Builder
    {
        public IGridSource? Source { get; set; }
        public List<GridColumnModel> Columns { get; set; } = [];
        public GridSelectionMode Mode { get; set; } = GridSelectionMode.Extended;
        public double Width { get; set; } = 600;
        public double Height { get; set; } = 400;
        public int Pumps { get; set; } = 1;
        public int? PrefetchMargin { get; set; }

        /// <summary>Sets the fixture pieces in one call (source is kept from
        /// <see cref="GridFixtures.Make"/> unless given again here).</summary>
        public Builder With(
            IGridSource? source,
            List<GridColumnModel>? columns = null,
            double width = 600,
            double height = 400,
            int pumps = 1,
            int? prefetchMargin = null)
        {
            Source = source ?? Source;
            Columns = columns ?? Columns;
            Width = width;
            Height = height;
            Pumps = pumps;
            PrefetchMargin = prefetchMargin ?? PrefetchMargin;
            return this;
        }

        /// <summary>Shows the grid in the headless window and pumps the dispatcher —
        /// the exact sequence every fixture uses, centralised so the boot order and
        /// pump policy live in exactly one place.</summary>
        public SpotGrid Build()
        {
            var grid = new SpotGrid
            {
                Columns = Columns,
                Source = Source,
                SelectionMode = Mode,
            };
            if (PrefetchMargin is { } margin)
            {
                grid.PrefetchMargin = margin;
            }

            TestHost.Show(grid, Width, Height);
            for (var i = 0; i < Pumps; i++)
            {
                TestHost.Pump();
            }

            return grid;
        }
    }

    /// <summary>Starts a fixture with the given source (null: no source yet) and
    /// selection mode.</summary>
    public static Builder Make(IGridSource? source = null, GridSelectionMode mode = GridSelectionMode.Extended)
        => new() { Source = source, Mode = mode };

    /// <summary>The plain pixel columns most fixtures use (<c>C0..C{count-1}</c>,
    /// width 100/90/80 by column).</summary>
    public static List<GridColumnModel> PixelColumns(int count, double width = 100, string prefix = "C")
    {
        var columns = new List<GridColumnModel>(count);
        for (var i = 0; i < count; i++)
        {
            columns.Add(new GridColumnModel { Header = $"{prefix}{i}", Width = GridColumnWidth.Pixels(width) });
        }

        return columns;
    }

    /// <summary>The <c>[start, start + count)</c> slice of the canonical row shape
    /// (<c>r{r}</c> key, first cell "row {r}") — the deferred source's completion
    /// payload; the shape is defined once, shared with the direct fixtures.</summary>
    public static List<GridRow> MakeRowsRange(int start, int count)
    {
        var rows = new List<GridRow>(count);
        for (var r = start; r < start + count; r++)
        {
            var cells = new GridCell[8];
            for (var c = 0; c < 8; c++)
            {
                cells[c] = new GridCell(c == 0 ? $"row {r}" : $"c{c}·{r}");
            }

            rows.Add(GridRow.Of($"r{r}", cells));
        }

        return rows;
    }
}