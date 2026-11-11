using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.VisualTree;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.ChildTable;

/// <summary>Where a paste reads its text from. The default reads the window's clipboard.</summary>
public interface IChildTableClipboard
{
    /// <summary>The clipboard's text, or null when it holds none.</summary>
    Task<string?> GetTextAsync();
}

/// <summary>Raised when an operation ran or was refused.</summary>
/// <param name="operation">What was asked ("delete rows", "paste").</param>
/// <param name="result">The outcome.</param>
public sealed class ChildTableOperationEventArgs(string operation, ChildTableResult result) : EventArgs
{
    /// <summary>What was asked.</summary>
    public string Operation { get; } = operation;

    /// <summary>The outcome.</summary>
    public ChildTableResult Result { get; } = result;
}

/// <summary>
/// The editable child-table control: the grid over a <see cref="ChildTableModel"/>. It turns
/// keys into model calls and the model's answers back into cursor, selection and scrolling, and
/// claims no state the grid does not publish: it drives the grid only through its public API.
///
/// <para>Keys (not while a cell editor is open): Enter edits the cell; on a cell that cannot be
/// edited it goes down, and on the last cell it adds a row. Ctrl+I inserts above, Ctrl+Shift+I
/// below, Ctrl+Delete deletes the selected rows (the cursor row without a selection), Alt+Up and
/// Alt+Down move them, Ctrl+D duplicates them, Ctrl+Shift+D fills the column down, Ctrl+V pastes
/// tab-separated text, Ctrl+Z undoes and Ctrl+Y or Ctrl+Shift+Z redoes. In a cell editor Enter
/// commits and goes down (adding a row after the last cell), Tab and Shift+Tab commit and go to
/// the next or previous editable cell, Up and Down commit and move, Ctrl+Enter commits and
/// stays, Escape cancels. A table that is not editable claims none of the editing keys.</para>
/// </summary>
public sealed class SpotChildTable : Decorator, IDisposable
{
    private readonly ChildTableEditor _editor;
    private bool _disposed;

    /// <summary>Creates the control over one child table of a draft.</summary>
    /// <param name="draft">The parent document's draft.</param>
    /// <param name="tableName">The table field's name.</param>
    /// <param name="view">The view definition (columns and totals); null shows every visible field.</param>
    /// <param name="lookups">Builds the lookup source of a link cell from the row's context.</param>
    /// <param name="lookupDelay">The lookup's type-ahead delay; null keeps the control's own.</param>
    public SpotChildTable(
        DocumentDraft draft,
        string tableName,
        ChildTableView? view = null,
        Func<ChildTableLookupRequest, ILookupSource?>? lookups = null,
        ILookupDelay? lookupDelay = null)
        : this(new ChildTableModel(draft, tableName, view), lookups, lookupDelay)
    {
    }

    /// <summary>Creates the control over a model (the control disposes it).</summary>
    public SpotChildTable(
        ChildTableModel model,
        Func<ChildTableLookupRequest, ILookupSource?>? lookups = null,
        ILookupDelay? lookupDelay = null)
    {
        ArgumentNullException.ThrowIfNull(model);
        Model = model;
        _editor = new ChildTableEditor(model, lookups, lookupDelay);
        Grid = new SpotGrid
        {
            Columns = model.Columns.Select(ToGridColumn).ToList(),
            Source = model.Source,
            SelectionMode = GridSelectionMode.Extended,
            // The whole table is held, so the grid fetches all of it in one window: every row has
            // its real key, and a selection is keyed by the row, never by a position.
            PrefetchMargin = 4096,
        };
        Avalonia.Automation.AutomationProperties.SetName(Grid, model.Table.Field.Label);
        Child = Grid;
        Grid.EditStarted += OnEditStarted;
        Grid.EditFinished += OnEditFinished;
        Grid.EditBeginning += OnEditBeginning;
        Grid.GridKeyDown += OnGridKey;
        _editor.ExitRequested += OnEditorExit;
        model.EditabilityChanged += OnEditabilityChanged;
        model.TotalsChanged += OnTotalsChanged;
        ApplyEditability();
        UpdateFooter();
    }

    /// <summary>The table model.</summary>
    public ChildTableModel Model { get; }

    /// <summary>The grid. Its public API is for reading; the table drives it.</summary>
    public SpotGrid Grid { get; }

    /// <summary>Where Ctrl+V reads from; the window's clipboard when null.</summary>
    public IChildTableClipboard? Clipboard { get; set; }

    /// <summary>
    /// The form's history, when the host gives the table one. Ctrl+Z, Ctrl+Shift+Z and Ctrl+Y — and
    /// the public <see cref="Undo"/> and <see cref="Redo"/> — then act on the document's one history,
    /// which replays this table's row journal itself, and the table's own journal is left alone. The
    /// key is still marked handled and the outcome still goes out through
    /// <see cref="OperationCompleted"/>, so the host's status text and focus follow the same path.
    /// Null (the default) is a table used on its own: it undoes and redoes its own rows, exactly as
    /// before.
    /// </summary>
    public IChildTableHistory? History { get; set; }

    /// <summary>The label drawn in the totals row's first cell.</summary>
    public string TotalsLabel { get; set; } = "Total";

    /// <summary>Raised after an operation ran or was refused.</summary>
    public event EventHandler<ChildTableOperationEventArgs>? OperationCompleted;

    /// <summary>The host's write right on the document (forwards to the model).</summary>
    public bool CanWrite
    {
        get => Model.CanWrite;
        set => Model.CanWrite = value;
    }

    /// <summary>The open cell editor's message after a refused commit, or null.</summary>
    public string? EditorErrorText => CurrentHost?.ErrorText;

    /// <summary>The open cell editor's host, or null.</summary>
    public CellEditorHost? CurrentHost => Grid.IsEditing ? Grid.ActiveEditor as CellEditorHost : null;

    // The grid hosts an open editor in its own overlay layer (a visual child of the grid) and takes it
    // out before it announces that the edit finished, so the table remembers the host to put it to rest.
    private CellEditorHost? _openHost;

    private void OnEditStarted(object? sender, GridEditStartedEventArgs e) => _openHost = e.Editor as CellEditorHost;

    private void OnEditFinished(object? sender, GridEditFinishedEventArgs e)
    {
        if (_openHost is { } host)
        {
            _openHost = null;
            _editor.Recycle(host);
        }
    }

    private static GridColumnModel ToGridColumn(ChildTableColumnInfo info) => new()
    {
        Header = info.Field.Label,
        Width = info.Spec.Width ?? CellFormatter.DefaultWidth(info.Field),
        DefaultAlignment = CellFormatter.Align(info.Field),
        IsSortable = false,
    };

    // ---- editability ------------------------------------------------------------------

    private void OnEditabilityChanged(object? sender, EventArgs e) => ApplyEditability();

    private void ApplyEditability()
    {
        if (Model.IsEditable)
        {
            Grid.EditorFactory ??= _editor;
            return;
        }

        // The right, the document state or a permission went away: an open editor must not
        // commit, and none may open.
        if (Grid.IsEditing)
        {
            Grid.EndEdit(false);
        }

        Grid.EditorFactory = null;
    }

    private void OnEditBeginning(object? sender, GridEditBeginningEventArgs e)
        => e.Cancel = Model.CellRefusal(e.RowIndex, e.ColumnIndex) != ChildTableRefusal.None;

    private void OnTotalsChanged(object? sender, EventArgs e) => UpdateFooter();

    private void UpdateFooter()
        => Grid.Footer = Model.Totals.HasTotals ? Model.Totals.BuildFooter(TotalsLabel) : null;

    // ---- operations (public so a toolbar or a test can call them) -------------------------------

    /// <summary>Finishes the open cell edit; false when its commit was refused (it stays open).</summary>
    private bool CommitOpenEditor() => !Grid.IsEditing || Grid.EndEdit(true);

    private ChildTableResult Run(string operation, Func<ChildTableResult> action)
    {
        if (_disposed)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NoWriteRight);
        }

        // SetCursor does not close an editor: finish the cell first, and stop if it will not commit.
        if (!CommitOpenEditor())
        {
            var blocked = ChildTableResult.Refused(ChildTableRefusal.Invalid, "Finish or cancel the cell you are editing first.");
            OperationCompleted?.Invoke(this, new ChildTableOperationEventArgs(operation, blocked));
            return blocked;
        }

        var result = action();
        if (result.Succeeded)
        {
            Apply(result);
        }

        OperationCompleted?.Invoke(this, new ChildTableOperationEventArgs(operation, result));
        return result;
    }

    private void Apply(ChildTableResult result)
    {
        if (result.Cursor is { } cursor && Model.RowCount > 0)
        {
            var column = cursor.Column >= 0 ? cursor.Column : Math.Max(0, Grid.GridCellCursor.ColumnIndex);
            Grid.ScrollIntoView(cursor.Row, column);
            Grid.SetCursor(cursor.Row, column);
        }

        if (result.Selection is { } selection)
        {
            Grid.SetSelection(selection);
        }
    }

    /// <summary>The rows an operation acts on: the selection when it holds the cursor row,
    /// otherwise the cursor row alone, so a stale selection never decides what is deleted.</summary>
    private List<int> EffectiveRows()
    {
        var cursor = Grid.GridCellCursor;
        var selected = Grid.SelectedRows;
        if (cursor.HasValue)
        {
            return selected.Contains(cursor.RowIndex) ? [.. selected] : [cursor.RowIndex];
        }

        return [.. selected];
    }

    private int CursorColumn => Math.Max(0, Grid.GridCellCursor.ColumnIndex);

    /// <summary>Appends a row and goes to it.</summary>
    public ChildTableResult AddRow() => Run("add row", Model.AddRow);

    /// <summary>Inserts a row above the cursor row.</summary>
    public ChildTableResult InsertAbove()
        => Run("insert row", () => Model.InsertRow(Grid.GridCellCursor.HasValue ? Grid.GridCellCursor.RowIndex : 0));

    /// <summary>Inserts a row below the cursor row.</summary>
    public ChildTableResult InsertBelow()
        => Run("insert row", () => Model.InsertRow(Grid.GridCellCursor.HasValue ? Grid.GridCellCursor.RowIndex + 1 : Model.RowCount));

    /// <summary>Deletes the selected rows (the cursor row without a selection).</summary>
    public ChildTableResult DeleteSelected() => Run("delete rows", () => Model.DeleteRows(EffectiveRows()));

    /// <summary>Moves the selected rows one place up (<paramref name="delta"/> -1) or down (+1).</summary>
    public ChildTableResult MoveSelected(int delta)
        => Run("move rows", () => Model.MoveRows(EffectiveRows(), delta, Grid.GridCellCursor.RowIndex));

    /// <summary>Duplicates the selected rows under the last of them.</summary>
    public ChildTableResult DuplicateSelected() => Run("duplicate rows", () => Model.DuplicateRows(EffectiveRows()));

    /// <summary>Fills the cursor column down from the first selected row (or from the row above).</summary>
    public ChildTableResult FillDown() => Run("fill down", () => Model.FillDown(EffectiveRows(), CursorColumn));

    /// <summary>Undoes the form's last step when the table has a history, else the table's own last operation.</summary>
    public ChildTableResult Undo()
        => History is { } history ? Run("undo", history.Undo) : Run("undo", Model.Undo);

    /// <summary>Redoes the form's last undone step when the table has a history, else the table's own.</summary>
    public ChildTableResult Redo()
        => History is { } history ? Run("redo", history.Redo) : Run("redo", Model.Redo);

    /// <summary>Pastes tab-separated text at the cursor (appending below the last row).</summary>
    public ChildTableResult PasteText(string text)
        => Run("paste", () =>
        {
            var cursor = Grid.GridCellCursor;
            return cursor.HasValue
                ? Model.Paste(text, cursor.RowIndex, cursor.ColumnIndex)
                : Model.Paste(text, Model.RowCount, 0);
        });

    /// <summary>Pastes the clipboard's text at the cursor.</summary>
    public async Task<ChildTableResult> PasteAsync()
    {
        string? text;
        try
        {
            var clipboard = Clipboard ?? new WindowClipboard(this);
            text = await clipboard.GetTextAsync().ConfigureAwait(true);
        }
        catch (Exception error) when (error is InvalidOperationException or NotSupportedException or IOException)
        {
            var failed = ChildTableResult.Refused(ChildTableRefusal.Invalid, "The clipboard could not be read.");
            OperationCompleted?.Invoke(this, new ChildTableOperationEventArgs("paste", failed));
            return failed;
        }

        if (string.IsNullOrEmpty(text))
        {
            var empty = ChildTableResult.Refused(ChildTableRefusal.NothingToDo, "The clipboard holds no text.");
            OperationCompleted?.Invoke(this, new ChildTableOperationEventArgs("paste", empty));
            return empty;
        }

        return PasteText(text);
    }

    /// <summary>Puts the cursor on the first cell that carries an error (a cell error, or the first
    /// cell of a row with a row error). Returns false when no cell has one or a cell edit is still
    /// open and will not commit. Call it after a failed save.</summary>
    public bool FocusFirstError()
    {
        if (!CommitOpenEditor())
        {
            return false;
        }

        if (Grid.FindNextAnnotation(null, GridAnnotationSeverity.Error) is not { HasValue: true } found)
        {
            return false;
        }

        Grid.ScrollIntoView(found.RowIndex, found.ColumnIndex);
        Grid.SetCursor(found.RowIndex, found.ColumnIndex);
        Grid.SetSelection([found.RowIndex]);
        Grid.Focus();
        return true;
    }

    // ---- keys ------------------------------------------------------------------------

    private void OnGridKey(object? sender, GridKeyEventArgs e)
    {
        // The args object is the grid's and is reused: read it now, keep nothing.
        var key = e.Key;
        var modifiers = e.KeyModifiers;
        if (e.IsEditing)
        {
            return;
        }

        var control = modifiers.HasFlag(KeyModifiers.Control) || modifiers.HasFlag(KeyModifiers.Meta);
        var shift = modifiers.HasFlag(KeyModifiers.Shift);
        var alt = modifiers.HasFlag(KeyModifiers.Alt);
        var editable = Model.IsEditable;

        switch (key)
        {
            case Key.Enter when modifiers == KeyModifiers.None:
                e.Handled = editable && EnterOnPlainCell();
                return;
            case Key.I when control && !alt:
                e.Handled = true;
                _ = shift ? InsertBelow() : InsertAbove();
                return;
            case Key.Delete when control && !alt && !shift:
                e.Handled = true;
                _ = DeleteSelected();
                return;
            case Key.Up or Key.Down when alt && !control && !shift && editable:
                // Read-only: Alt+Down stays the grid's "show this cell's note" key.
                e.Handled = true;
                _ = MoveSelected(key == Key.Up ? -1 : 1);
                return;
            case Key.D when control && !alt:
                e.Handled = true;
                _ = shift ? FillDown() : DuplicateSelected();
                return;
            case Key.V when control && !alt && !shift:
                e.Handled = true;
                _ = PasteAsync();
                return;
            case Key.Z when control && !alt:
                e.Handled = true;
                _ = shift ? Redo() : Undo();
                return;
            case Key.Y when control && !alt && !shift:
                e.Handled = true;
                _ = Redo();
                return;
            default:
                return;
        }
    }

    /// <summary>Enter on a cell the grid will edit is the grid's; on one it cannot, it goes down,
    /// and on the last cell it adds a row. Returns true when the key was used.</summary>
    private bool EnterOnPlainCell()
    {
        var cursor = Grid.GridCellCursor;
        if (!cursor.HasValue || Model.CellRefusal(cursor.RowIndex, cursor.ColumnIndex) == ChildTableRefusal.None)
        {
            return false;
        }

        Advance(cursor.RowIndex, cursor.ColumnIndex, edit: true);
        return true;
    }

    // ---- leaving a cell editor ---------------------------------------------------------

    private void OnEditorExit(object? sender, ChildTableEditorExitEventArgs e)
    {
        var row = e.Row;
        var column = e.Column;
        if (e.Exit == ChildTableEditorExit.Cancel)
        {
            Grid.EndEdit(false);
            Grid.Focus();
            return;
        }

        if (!Grid.EndEdit(true))
        {
            return; // the commit was refused: the editor stays open and the cell says why
        }

        switch (e.Exit)
        {
            case ChildTableEditorExit.CtrlEnter:
                Grid.SetCursor(row, column);
                Grid.Focus();
                break;
            case ChildTableEditorExit.Enter:
                Advance(row, column, edit: true);
                break;
            case ChildTableEditorExit.Down:
                if (row + 1 < Model.RowCount)
                {
                    GoTo(row + 1, column, edit: true);
                }
                else
                {
                    GoTo(row, column, edit: false);
                }

                break;
            case ChildTableEditorExit.Up:
                GoTo(Math.Max(0, row - 1), column, edit: true);
                break;
            case ChildTableEditorExit.Tab:
                if (Model.NextEditable(row, column, forward: true) is { } next)
                {
                    GoTo(next.Row, next.Column, edit: true);
                }
                else if (Model.IsEditable)
                {
                    AddAndEdit();
                }

                break;
            case ChildTableEditorExit.ShiftTab:
                if (Model.NextEditable(row, column, forward: false) is { } previous)
                {
                    GoTo(previous.Row, previous.Column, edit: true);
                }
                else
                {
                    GoTo(row, column, edit: false);
                }

                break;
            default:
                break;
        }
    }

    /// <summary>Enter's move: down one row; on the last row, on the last editable cell, add a row.</summary>
    private void Advance(int row, int column, bool edit)
    {
        if (row + 1 < Model.RowCount)
        {
            GoTo(row + 1, column, edit);
            return;
        }

        var lastEditable = Model.NextEditable(row, column, forward: true) is not { Row: var r } || r != row;
        if (lastEditable && Model.IsEditable)
        {
            AddAndEdit();
        }
        else
        {
            GoTo(row, column, edit: false);
        }
    }

    private void AddAndEdit()
    {
        var result = Model.AddRow();
        OperationCompleted?.Invoke(this, new ChildTableOperationEventArgs("add row", result));
        if (!result.Succeeded || result.Cursor is not { } cursor)
        {
            return;
        }

        GoTo(cursor.Row, Math.Max(0, cursor.Column), edit: true);
    }

    private void GoTo(int row, int column, bool edit)
    {
        Grid.ScrollIntoView(row, column);
        Grid.SetCursor(row, column);
        Grid.SetSelection([row]);
        if (!edit || !Grid.BeginEdit(row, column))
        {
            Grid.Focus();
        }
    }

    /// <summary>Unwires the table from the draft.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        Grid.EditStarted -= OnEditStarted;
        Grid.EditFinished -= OnEditFinished;
        Grid.EditBeginning -= OnEditBeginning;
        Grid.GridKeyDown -= OnGridKey;
        _editor.ExitRequested -= OnEditorExit;
        Model.EditabilityChanged -= OnEditabilityChanged;
        Model.TotalsChanged -= OnTotalsChanged;
        Grid.Dispose();
        Model.Dispose();
    }

    private sealed class WindowClipboard(Control owner) : IChildTableClipboard
    {
        public async Task<string?> GetTextAsync()
        {
            var clipboard = TopLevel.GetTopLevel(owner)?.Clipboard;
            return clipboard is null ? null : await Avalonia.Input.Platform.ClipboardExtensions.TryGetTextAsync(clipboard).ConfigureAwait(true);
        }
    }
}
