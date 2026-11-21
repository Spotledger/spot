namespace Spotledger.Client.Grid;

/// <summary>Which area of the grid a point belongs to (hit-testing).</summary>
public enum GridArea
{
    /// <summary>Empty space.</summary>
    None,
    /// <summary>The pinned header band.</summary>
    Header,
    /// <summary>A data row.</summary>
    Cell,
    /// <summary>Past the last row.</summary>
    Below,
    /// <summary>The header edge (the resize grip area) of the column to the left.</summary>
    HeaderEdge,
}

/// <summary>A hit-test result: the area plus its row/column indices.</summary>
/// <param name="Area">Which region.</param>
/// <param name="ColumnIndex">The column model index, or -1.</param>
/// <param name="RowIndex">The source row index, or -1.</param>
public readonly record struct GridHitTest(GridArea Area, int ColumnIndex = -1, int RowIndex = -1);

/// <summary>A cell cursor position: source row index + column model index.</summary>
/// <param name="RowIndex">Source row index, or -1 for none.</param>
/// <param name="ColumnIndex">Column model index, or -1 for none.</param>
/// <param name="Origin">Where the move came from (keyboard vs pointer).</param>
public readonly record struct GridCursor(int RowIndex = -1, int ColumnIndex = -1, GridCursorOrigin Origin = GridCursorOrigin.None)
{
    /// <summary>True when a cell is focused.</summary>
    public bool HasValue => RowIndex >= 0 && ColumnIndex >= 0;
}

/// <summary>Whether a cursor move came from the keyboard (grid handles it) or the mouse.</summary>
public enum GridCursorOrigin
{
    /// <summary>No cursor.</summary>
    None,
    /// <summary>Keyboard navigation.</summary>
    Keyboard,
    /// <summary>Mouse click/activation.</summary>
    Mouse,
}