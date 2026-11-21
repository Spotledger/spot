using System.ComponentModel;
using Avalonia.Controls;

namespace Spotledger.Client.Grid;

/// <summary>
/// The edit hook WP-UI-08.T3 consumes. The grid positions, commits and cancels an edit;
/// the host supplies the editor. The grid itself never ships an editor: with no factory
/// set, BeginEdit is a no-op that returns false.
/// </summary>
public interface ICellEditorFactory
{
    /// <summary>Creates an editor control for the cell, or null when this cell is not
    /// editable. The editor must not be attached to any visual parent yet.</summary>
    Control? CreateEditor(int rowIndex, int columnIndex, GridRow row, GridColumnModel column);

    /// <summary>Reads the editor's current proposed value (a display string — cell
    /// values are strings end to end). The grid calls this to preview and record.</summary>
    string GetValue(Control editor);

    /// <summary>Commits: takes the grid-proposed value (from <see cref="GetValue"/>) and
    /// pushes it to the source in one call. Return false to keep the editor open
    /// (a validation failure).</summary>
    bool TryCommit(int rowIndex, int columnIndex, string proposedValue, Control editor);

    /// <summary>Cancels: discard whatever the editor holds and close it.</summary>
    void Cancel(int rowIndex, int columnIndex, Control editor);
}

/// <summary>Internal live-edit state exposed to tests through the <see cref="SpotGrid"/>
/// partials (InternalsVisibleTo on the Grid assembly).</summary>
public sealed class GridEditSession
{
    /// <summary>The row being edited (source index).</summary>
    public int RowIndex { get; internal set; }

    /// <summary>The column being edited (column model index).</summary>
    public int ColumnIndex { get; internal set; }

    /// <summary>The editor control currently hosted.</summary>
    public Control? Editor { get; internal set; }

    /// <summary>True while an editor is open.</summary>
    public bool IsOpen => Editor is not null;
}

/// <summary>The spreadsheet cursor moves the edit session supports on commit/cancel.</summary>
public enum GridEditMove
{
    /// <summary>No move.</summary>
    None,

    /// <summary>Commit and move to the next cell; with shift, the previous (Tab).</summary>
    TabNext,

    /// <summary>Commit; the move is host-driven (Enter stays on the row).</summary>
    EnterCommit,

    /// <summary>Cancel and close (Esc).</summary>
    EscapeCancel,
}

/// <summary>Raised when <see cref="SpotGrid.BeginEdit"/> starts an editor (the host may
/// seed the editor's text from the cell).</summary>
/// <param name="rowIndex">Source row index.</param>
/// <param name="columnIndex">Column model index.</param>
/// <param name="editor">The editor control (not yet parented).</param>
/// <param name="initialText">The cell's current display string.</param>
public sealed class GridEditStartedEventArgs(
    int rowIndex, int columnIndex, Control editor, string initialText) : EventArgs
{
    /// <summary>Source row index.</summary>
    public int RowIndex { get; } = rowIndex;

    /// <summary>Column model index.</summary>
    public int ColumnIndex { get; } = columnIndex;

    /// <summary>The editor control (not yet parented).</summary>
    public Control Editor { get; } = editor;

    /// <summary>The cell's current display string.</summary>
    public string InitialText { get; } = initialText;
}

/// <summary>Raised when an editor closed: commit (a value was pushed) or cancel.</summary>
/// <param name="rowIndex">Source row index.</param>
/// <param name="columnIndex">Column model index.</param>
/// <param name="committed">True when the value was pushed to the source.</param>
/// <param name="committedValue">The string pushed, or null on cancel.</param>
public sealed class GridEditFinishedEventArgs(
    int rowIndex, int columnIndex, bool committed, string? committedValue) : EventArgs
{
    /// <summary>Source row index.</summary>
    public int RowIndex { get; } = rowIndex;

    /// <summary>Column model index.</summary>
    public int ColumnIndex { get; } = columnIndex;

    /// <summary>True when the value was pushed to the source.</summary>
    public bool Committed { get; } = committed;

    /// <summary>The string pushed, or null on cancel.</summary>
    public string? CommittedValue { get; } = committedValue;
}

/// <summary>Raised from <see cref="SpotGrid.BeginEdit"/> so the host can veto edits it
/// does not allow (row state, permissions). Veto by setting <see cref="Cancel"/>.</summary>
/// <param name="rowIndex">Source row index.</param>
/// <param name="columnIndex">Column model index.</param>
public sealed class GridEditBeginningEventArgs(int rowIndex, int columnIndex) : EventArgs
{
    /// <summary>Source row index.</summary>
    public int RowIndex { get; } = rowIndex;

    /// <summary>Column model index.</summary>
    public int ColumnIndex { get; } = columnIndex;

    /// <summary>Set true to refuse the edit.</summary>
    public bool Cancel { get; set; }
}

/// <summary>A convenience base the T3 factory host may derive: wraps a
/// <see cref="TextBox"/>-backed editor, the common case.</summary>
public abstract class CellEditorFactoryBase : ICellEditorFactory
{
    /// <inheritdoc />
    public abstract Control? CreateEditor(int rowIndex, int columnIndex, GridRow row, GridColumnModel column);

    /// <inheritdoc />
    public abstract string GetValue(Control editor);

    /// <inheritdoc />
    public abstract bool TryCommit(int rowIndex, int columnIndex, string proposedValue, Control editor);

    /// <inheritdoc />
    public abstract void Cancel(int rowIndex, int columnIndex, Control editor);

    /// <summary>Raised when a property of this factory changes (reserved for T3).</summary>
    public event PropertyChangedEventHandler? PropertyChanged;

    /// <summary>Raises <see cref="PropertyChanged"/> for <paramref name="name"/>.</summary>
    protected void RaiseChanged(string name) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}