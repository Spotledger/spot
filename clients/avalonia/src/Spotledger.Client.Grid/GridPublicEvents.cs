using Avalonia.Input;

namespace Spotledger.Client.Grid;

/// <summary>Raised by <see cref="SpotGrid.CursorChanged"/> when the focused cell moves.</summary>
public sealed class GridCursorChangedEventArgs : EventArgs
{
    internal GridCursorChangedEventArgs(GridCursor oldCursor, GridCursor newCursor)
    {
        Old = oldCursor;
        New = newCursor;
    }

    /// <summary>The cursor before the move.</summary>
    public GridCursor Old { get; }

    /// <summary>The cursor after the move.</summary>
    public GridCursor New { get; }
}

/// <summary>
/// A key seen by the grid before its default handling (<see cref="SpotGrid.GridKeyDown"/>).
/// Set <see cref="Handled"/> to claim the key: the grid then does nothing else with it.
/// The grid reuses one instance per key press to avoid allocating; do not keep it.
/// </summary>
public sealed class GridKeyEventArgs : EventArgs
{
    internal GridKeyEventArgs()
    {
    }

    /// <summary>The key pressed.</summary>
    public Key Key { get; private set; }

    /// <summary>The modifier keys held.</summary>
    public KeyModifiers KeyModifiers { get; private set; }

    /// <summary>The cell cursor when the key arrived.</summary>
    public GridCursor Cursor { get; private set; }

    /// <summary>True while a cell editor is open.</summary>
    public bool IsEditing { get; private set; }

    /// <summary>The total row count when the key arrived.</summary>
    public int RowCount { get; private set; }

    /// <summary>Set to true to claim the key and suppress the grid's default handling.</summary>
    public bool Handled { get; set; }

    internal void Reset(Key key, KeyModifiers modifiers, GridCursor cursor, bool isEditing, int rowCount)
    {
        Key = key;
        KeyModifiers = modifiers;
        Cursor = cursor;
        IsEditing = isEditing;
        RowCount = rowCount;
        Handled = false;
    }
}
