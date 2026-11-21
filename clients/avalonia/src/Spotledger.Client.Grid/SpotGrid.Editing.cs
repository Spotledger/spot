using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.VisualTree;

namespace Spotledger.Client.Grid;

/// <summary>Editing half of <see cref="SpotGrid"/>: the cell-editing hook WP-UI-08.T3
/// consumes. The grid positions, commits and cancels; the host supplies the editor via
/// <see cref="ICellEditorFactory"/>. Tab/Enter/Esc move the cursor after closing like a
/// spreadsheet. No editor ships in this project.</summary>
public partial class SpotGrid
{
    private GridEditSession _session = new();

    /// <summary>The live edit session (tests assert over it).</summary>
    internal GridEditSession EditSession => _session;

    /// <summary>The control hosted by the open cell editor (the one the editor factory made),
    /// or null when no editor is open. It sits in the grid's overlay layer, a visual child of
    /// the grid, over its cell.</summary>
    public Control? ActiveEditor => _session.IsOpen ? _session.Editor : null;

    /// <summary>True while a cell editor is open.</summary>
    public bool IsEditing => _session.IsOpen;

    /// <summary>
    /// Opens an editor for the cell: asks the factory (a <see langword="null"/> return
    /// or no factory means the cell is not editable and nothing happens), raises
    /// <see cref="EditBeginning"/> so the host can veto, then hosts the editor inside a
    /// transparent overlay <see cref="Decorator"/> positioned over the cell. Returns
    /// true when an editor opened.
    /// </summary>
    public bool BeginEdit(int rowIndex, int columnIndex)
    {
        if (IsEditing)
        {
            CloseEditor(commit: false);
        }

        if (EditorFactory is not { } factory
            || Columns is not { } columns
            || columnIndex < 0 || columnIndex >= columns.Count
            || RowAt(rowIndex) is not { } row)
        {
            return false;
        }

        var beginning = new GridEditBeginningEventArgs(rowIndex, columnIndex);
        EditBeginning?.Invoke(this, beginning);
        if (beginning.Cancel)
        {
            return false;
        }

        var editor = factory.CreateEditor(rowIndex, columnIndex, row, columns[columnIndex]);
        if (editor is null)
        {
            return false;
        }

        var initialText = columnIndex < row.Cells.Count ? row.Cells[columnIndex].Text : string.Empty;
        _session = new GridEditSession
        {
            RowIndex = rowIndex,
            ColumnIndex = columnIndex,
            Editor = editor,
        };

        // Host the editor in the overlay so it draws above the custom-painted body and
        // receives keyboard input; the grid keeps painting underneath it.
        OverlayDecorator.Child = editor;
        PositionEditor();
        EditStarted?.Invoke(this, new GridEditStartedEventArgs(rowIndex, columnIndex, editor, initialText));
        AttachEditorKeys(editor);
        editor.Focus();
        InvalidateVisual();
        return true;
    }

    private void PositionEditor()
    {
        if (_session.Editor is not { } editor || _columnWidths is not { } widths || _columnOffsets is not { } offsets)
        {
            return;
        }

        var columnIndex = _session.ColumnIndex;
        var x = ColumnX(columnIndex);
        var width = columnIndex < widths.Length ? widths[columnIndex] : 0;
        var y = RowY(_session.RowIndex);
        editor.HorizontalAlignment = Avalonia.Layout.HorizontalAlignment.Left;
        editor.VerticalAlignment = Avalonia.Layout.VerticalAlignment.Top;
        editor.Margin = new Thickness(x, y, 0, 0);
        editor.Width = width;
        editor.Height = RowHeight;
    }

    /// <summary>Positions and styles a text editor over cell (row, column) — the grid's
    /// half of the hook: geometry is the grid's business, content the factory's.</summary>
    internal void PlaceEditor(Control editor, int rowIndex, int columnIndex)
    {
        _session.RowIndex = rowIndex;
        _session.ColumnIndex = columnIndex;
        _session.Editor = editor;
        OverlayDecorator.Child = editor;
        PositionEditor();
    }

    private Decorator? _overlayDecorator;

    /// <summary>
    /// The overlay layer that hosts open editors above the painted body: a real visual (and
    /// logical) child of the grid, created and attached on first use, kept for the grid's life
    /// and arranged over the whole grid. An open editor is its <see cref="Decorator.Child"/>,
    /// positioned over its cell; the layer draws nothing itself and is not hit-testable where
    /// it holds no editor. ChildTable reads the open editor's host through it.
    /// </summary>
    internal Decorator OverlayDecorator
    {
        get
        {
            if (_overlayDecorator is null)
            {
                _overlayDecorator = new Decorator { IsHitTestVisible = true };
                LogicalChildren.Add(_overlayDecorator);
                VisualChildren.Add(_overlayDecorator);
                InvalidateMeasure();
            }

            return _overlayDecorator;
        }
    }

    /// <summary>True when the routed event came up from an editor hosted in the overlay: the
    /// grid's own pointer and key handling belongs to the painted body, not to the editor.</summary>
    private bool IsFromEditor(Avalonia.Interactivity.RoutedEventArgs e)
        => _overlayDecorator is { } overlay
            && e.Source is Visual source
            && (ReferenceEquals(source, overlay) || overlay.IsVisualAncestorOf(source));

    /// <summary>Commits or cancels the open editor and optionally moves the cursor like
    /// a spreadsheet: Tab → next cell, Enter → commit and stay (host may move), Esc →
    /// close with no move.</summary>
    internal void CloseEditor(bool commit, GridEditMove move = GridEditMove.None)
    {
        if (!IsEditing)
        {
            return;
        }

        var session = _session;
        var factory = EditorFactory;
        string? committedValue = null;
        var didCommit = false;
        if (factory is not null && session.Editor is { } editor)
        {
            if (commit)
            {
                var value = factory.GetValue(editor);
                didCommit = factory.TryCommit(session.RowIndex, session.ColumnIndex, value, editor);
                if (didCommit)
                {
                    committedValue = value;
                }
                else
                {
                    // A rejected commit is a validation failure: the editor stays open
                    // with its text untouched (no close, no event, no cursor move).
                    InvalidateVisual();
                    return;
                }
            }
            else
            {
                factory.Cancel(session.RowIndex, session.ColumnIndex, editor);
            }
        }

        if (session.Editor is { })
        {
            DetachEditorKeys(session.Editor);
            OverlayDecorator.Child = null;
            Focus();
        }

        _session = new GridEditSession();
        EditFinished?.Invoke(this, new GridEditFinishedEventArgs(
            session.RowIndex, session.ColumnIndex, didCommit, committedValue));

        SpreadsheetMove(didCommit, move);
        InvalidateVisual();
    }

    /// <summary>The spreadsheet cursor move after a commit/cancel.</summary>
    private void SpreadsheetMove(bool committed, GridEditMove move)
    {
        if (!move.HasFlag(GridEditMove.TabNext) || !committed)
        {
            return;
        }

        var column = CellCursor.ColumnIndex + 1;
        var row = CellCursor.RowIndex;
        if (column >= ColumnCount)
        {
            column = 0;
            row = Math.Min(TotalRows - 1, row + 1);
        }

        CellCursor = new GridCursor(row, column, GridCursorOrigin.Keyboard);
        EnsureCursorVisible();
        InvalidateVisual();
    }

    /// <summary>Wires the editor keys: Tab/Enter commit, Esc cancels. Key handlers are
    /// attached to the editor while it lives inside the overlay.</summary>
    private void AttachEditorKeys(Control editor)
    {
        editor.KeyDown += OnEditorKeyDown;
    }

    private void DetachEditorKeys(Control editor)
    {
        editor.KeyDown -= OnEditorKeyDown;
    }

    private void OnEditorKeyDown(object? sender, KeyEventArgs e)
    {
        if (e.Key is not (Key.Tab or Key.Enter or Key.Escape))
        {
            return;
        }

        e.Handled = true;
        CloseEditor(
            commit: e.Key != Key.Escape,
            e.Key switch
            {
                Key.Tab => GridEditMove.TabNext,
                Key.Enter => GridEditMove.EnterCommit,
                _ => GridEditMove.EscapeCancel,
            });
    }

    /// <summary>Called from arrange so an open editor keeps tracking its cell geometry
    /// (a resize behind the editor must not tear it out of position).</summary>
    internal void RepositionEditorIfOpen()
    {
        if (IsEditing)
        {
            PositionEditor();
        }
    }
}