namespace Spotledger.Client.Grid;

/// <summary>An immutable row handed over by an <see cref="IGridSource"/> window. The grid
/// never mutates it: cells are display strings drawn as-is (numerics stay the source's
/// business, ADR-12 — no floating-point arithmetic lives in the grid).</summary>
/// <param name="Key">An opaque identity the grid uses for selection, checking and change
/// bookkeeping across resets; it is never parsed.</param>
/// <param name="Cells">One <see cref="GridCell"/> per data column, index-aligned with the
/// grid's column model.</param>
/// <param name="IsChecked">The row's checked state when the grid shows a check column.</param>
public sealed record GridRow(object Key, IReadOnlyList<GridCell> Cells, bool IsChecked = false)
{
    /// <summary>Convenience factory for homogeneous rows.</summary>
    public static GridRow Of(object key, params GridCell[] cells) => new(key, cells);

    /// <summary>An optional note on the whole row, drawn as a marker in the row's first
    /// column (the grid has no row header). Defaults to none.</summary>
    public GridCellAnnotation? RowAnnotation { get; init; }
}

/// <summary>One draw-ready cell: a display string plus presentation, never a control.</summary>
/// <param name="Text">The exact string to draw; an empty string draws nothing.</param>
/// <param name="Alignment">Horizontal placement; defaults to the start edge.</param>
/// <param name="StyleClass">An optional style class key the source may set (e.g. a
/// semantic state name); the grid maps recognized classes to token brushes.</param>
public sealed record GridCell(
    string Text,
    GridCellAlignment Alignment = GridCellAlignment.Start,
    string? StyleClass = null)
{
    /// <summary>An optional note on this cell (severity and message): the grid draws a
    /// corner marker and the tooltip; the text is never changed. Defaults to none.</summary>
    public GridCellAnnotation? Annotation { get; init; }
}