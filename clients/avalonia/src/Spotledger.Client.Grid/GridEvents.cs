namespace Spotledger.Client.Grid;

/// <summary>Raised when Enter activates a focused row or the user double-clicks one.</summary>
/// <param name="rowIndex">The row's current source index.</param>
/// <param name="rowKey">The row's opaque key (stable across sorts/resets).</param>
public sealed class GridRowActivatedEventArgs(int rowIndex, object rowKey) : EventArgs
{
    /// <summary>The row's current source index.</summary>
    public int RowIndex { get; } = rowIndex;

    /// <summary>The row's opaque key (stable across sorts/resets).</summary>
    public object RowKey { get; } = rowKey;
}

/// <summary>Raised when a row's check state flips (check column click or Space).</summary>
/// <param name="rowIndex">The row's current source index.</param>
/// <param name="rowKey">The row's opaque key.</param>
/// <param name="isChecked">The new state.</param>
public sealed class GridRowCheckedChangedEventArgs(int rowIndex, object rowKey, bool isChecked) : EventArgs
{
    /// <summary>The row's current source index.</summary>
    public int RowIndex { get; } = rowIndex;

    /// <summary>The row's opaque key.</summary>
    public object RowKey { get; } = rowKey;

    /// <summary>The new state.</summary>
    public bool IsChecked { get; } = isChecked;
}

/// <summary>Raised after a column drag-resize or auto-fit changes a column's pixel width.</summary>
/// <param name="columnIndex">The resized column's index in the column model.</param>
/// <param name="newWidth">The new pixel width.</param>
public sealed class GridColumnResizedEventArgs(int columnIndex, double newWidth) : EventArgs
{
    /// <summary>The resized column's index in the column model.</summary>
    public int ColumnIndex { get; } = columnIndex;

    /// <summary>The new pixel width.</summary>
    public double NewWidth { get; } = newWidth;
}

/// <summary>Raised after a column reorder drag or keyboard move completes.</summary>
/// <param name="fromIndex">The layout index the column was at.</summary>
/// <param name="toIndex">The layout index it now sits at.</summary>
public sealed class GridColumnReorderedEventArgs(int fromIndex, int toIndex) : EventArgs
{
    /// <summary>The layout index the column was at.</summary>
    public int FromIndex { get; } = fromIndex;

    /// <summary>The layout index it now sits at.</summary>
    public int ToIndex { get; } = toIndex;
}

/// <summary>Raised when a header click completes a sort cycle step.</summary>
/// <param name="columnIndex">The sorted column.</summary>
/// <param name="direction">The new direction (None when the cycle closed).</summary>
public sealed class GridSortedEventArgs(int columnIndex, GridSortDirection direction) : EventArgs
{
    /// <summary>The sorted column.</summary>
    public int ColumnIndex { get; } = columnIndex;

    /// <summary>The new direction (None when the cycle closed).</summary>
    public GridSortDirection Direction { get; } = direction;
}