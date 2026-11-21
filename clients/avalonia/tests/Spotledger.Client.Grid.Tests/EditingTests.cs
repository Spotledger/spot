using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.VisualTree;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>The cell-editing hook: BeginEdit with/without factory, veto, commit,
/// cancel and the spreadsheet cursor moves.</summary>
public sealed class EditingTests
{
    private static SpotGrid MakeGrid(out InMemoryGridSource source)
    {
        // Editing fixtures name cells "c{2c}·r" (slot 0 = "c0·r", slot 1 = "c2·r", ...):
        // the asserts pin the exact cell text per model column, proving the grid hands
        // the factory the cell the edit targets.
        source = new InMemoryGridSource(GridFixture.MakeEditingRows(20, 3));
        return GridFixtures.Make(source)
            .With(source, new List<GridColumnModel>
            {
                new() { Header = "A", Width = GridColumnWidth.Pixels(120) },
                new() { Header = "B", Width = GridColumnWidth.Pixels(120) },
                new() { Header = "C", Width = GridColumnWidth.Pixels(120) },
            }, pumps: 2)
            .Build();
    }

    /// <summary>A TextBox-backed factory, the shape WP-UI-08.T3 will supply.</summary>
    private sealed class TextBoxFactory : ICellEditorFactory
    {
        public int Commits { get; private set; }
        public int Cancels { get; private set; }
        public bool Vetoable { get; set; }
        public bool RejectCommit { get; set; }
        public string? LastCommitted { get; private set; }

        public Control? CreateEditor(int rowIndex, int columnIndex, GridRow row, GridColumnModel column)
            => Vetoable ? null : new TextBox { Text = row.Cells[columnIndex].Text };

        public string GetValue(Control editor) => editor is TextBox box ? box.Text ?? string.Empty : string.Empty;

        public bool TryCommit(int rowIndex, int columnIndex, string proposedValue, Control editor)
        {
            if (RejectCommit)
            {
                return false;
            }

            Commits++;
            LastCommitted = proposedValue;
            return true;
        }

        public void Cancel(int rowIndex, int columnIndex, Control editor) => Cancels++;
    }

    [AvaloniaFact]
    public void WithoutFactoryBeginEditReturnsFalse()
    {
        var grid = MakeGrid(out _);
        Assert.False(grid.BeginEdit(0, 0));
        Assert.False(grid.IsEditing);
    }

    [AvaloniaFact]
    public void BeginEditOpensAnEditorAndPositionsIt()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new TextBoxFactory();

        Assert.True(grid.BeginEdit(2, 1));
        Assert.True(grid.IsEditing);
        Assert.Equal(2, grid.EditSession.RowIndex);
        Assert.Equal(1, grid.EditSession.ColumnIndex);
        Assert.NotNull(grid.EditSession.Editor);

        var editor = Assert.IsType<TextBox>(grid.EditSession.Editor);
        Assert.Equal("c2·2", editor.Text);
        Assert.Equal(120, editor.Width);
        Assert.True(editor.Margin.Left > 0, "the editor must sit over its cell");
    }

    [AvaloniaFact]
    public void FactoryReturningNullMeansNotEditable()
    {
        var grid = MakeGrid(out _);
        var factory = new TextBoxFactory { Vetoable = true };
        grid.EditorFactory = factory;
        Assert.False(grid.BeginEdit(1, 0));
        Assert.False(grid.IsEditing);
    }

    [AvaloniaFact]
    public void EditBeginningVetoBlocksTheEdit()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new TextBoxFactory();
        grid.EditBeginning += (_, e) => e.Cancel = true;
        Assert.False(grid.BeginEdit(1, 0));
    }

    [AvaloniaFact]
    public async Task EscCancelsAndDoesNotTouchTheSource()
    {
        var grid = MakeGrid(out var source);
        var factory = new TextBoxFactory();
        grid.EditorFactory = factory;
        var finished = new List<bool>();
        grid.EditFinished += (_, e) => finished.Add(e.Committed);

        Assert.True(grid.BeginEdit(0, 0));
        var editor = Assert.IsType<TextBox>(grid.EditSession.Editor!);
        editor.Text = "changed";
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Escape, RoutedEvent = InputElement.KeyDownEvent });

        Assert.False(grid.IsEditing);
        Assert.Equal(0, factory.Commits);
        Assert.Equal(1, factory.Cancels);
        Assert.Single(finished);
        Assert.False(finished[0]);
        var committed = await source.GetWindow(0, 1, CancellationToken.None);
        Assert.Equal("c0·0", committed[0].Cells[0].Text);
    }

    [AvaloniaFact]
    public void EnterCommitsAndFinishes()
    {
        var grid = MakeGrid(out var source);
        var factory = new TextBoxFactory();
        grid.EditorFactory = factory;

        Assert.True(grid.BeginEdit(0, 0));
        var editor = Assert.IsType<TextBox>(grid.EditSession.Editor!);
        editor.Text = "edited";
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Enter, RoutedEvent = InputElement.KeyDownEvent });

        Assert.False(grid.IsEditing);
        Assert.Equal(1, factory.Commits);
        Assert.Equal("edited", factory.LastCommitted);
    }

    [AvaloniaFact]
    public void CommitFailureKeepsTheEditorOpen()
    {
        var grid = MakeGrid(out _);
        var factory = new TextBoxFactory { RejectCommit = true };
        grid.EditorFactory = factory;

        Assert.True(grid.BeginEdit(0, 0));
        var editor = Assert.IsType<TextBox>(grid.EditSession.Editor!);
        editor.Text = "whatever";
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Enter, RoutedEvent = InputElement.KeyDownEvent });

        Assert.True(grid.IsEditing, "a failed commit must keep the editor open");
    }

    [AvaloniaFact]
    public void TabCommitsAndMovesToTheNextCell()
    {
        var grid = MakeGrid(out _);
        var factory = new TextBoxFactory();
        grid.EditorFactory = factory;
        grid.CellCursor = new GridCursor(3, 0, GridCursorOrigin.Keyboard);

        Assert.True(grid.BeginEdit(3, 0));
        var editor = Assert.IsType<TextBox>(grid.EditSession.Editor!);
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Tab, RoutedEvent = InputElement.KeyDownEvent });

        Assert.False(grid.IsEditing);
        Assert.Equal(1, factory.Commits);
        Assert.Equal(1, grid.CellCursor.ColumnIndex);
        Assert.Equal(3, grid.CellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void TabOnTheLastColumnWrapsToTheNextRow()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new TextBoxFactory();
        grid.CellCursor = new GridCursor(3, 2, GridCursorOrigin.Keyboard);

        Assert.True(grid.BeginEdit(3, 2));
        var editor = Assert.IsType<TextBox>(grid.EditSession.Editor!);
        editor.RaiseEvent(new KeyEventArgs { Key = Key.Tab, RoutedEvent = InputElement.KeyDownEvent });

        Assert.Equal(0, grid.CellCursor.ColumnIndex);
        Assert.Equal(4, grid.CellCursor.RowIndex);
    }

    [AvaloniaFact]
    public void OpeningASecondEditorClosesTheFirst()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new TextBoxFactory();
        Assert.True(grid.BeginEdit(0, 0));
        Assert.True(grid.BeginEdit(5, 1));
        Assert.Equal(5, grid.EditSession.RowIndex);
        Assert.Equal(1, grid.EditSession.ColumnIndex);
    }

    [AvaloniaFact]
    public void AnOpenedEditorIsInTheVisualTreeOverItsCellAndHoldsFocus()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new TextBoxFactory();

        Assert.True(grid.BeginEdit(2, 1));
        TestHost.Pump();

        var editor = Assert.IsType<TextBox>(grid.ActiveEditor);
        Assert.True(grid.IsVisualAncestorOf(editor), "the editor must be in the grid's visual tree");
        Assert.Same(grid, editor.GetVisualAncestors().OfType<SpotGrid>().First());
        // The overlay layer is arranged at the grid's origin, so the editor's bounds are in grid coordinates.
        Assert.Equal(grid.ColumnX(1), editor.Bounds.X, 1);
        Assert.Equal(grid.RowY(2), editor.Bounds.Y, 1);
        Assert.Equal(editor.Width, editor.Bounds.Width, 1);
        Assert.Equal(grid.RowHeight, editor.Bounds.Height, 1);
        Assert.Same(editor, TopLevel.GetTopLevel(grid)!.FocusManager!.GetFocusedElement());

        Assert.True(grid.EndEdit(commit: false));
        TestHost.Pump();
        Assert.False(grid.IsVisualAncestorOf(editor));
        Assert.Same(grid, TopLevel.GetTopLevel(grid)!.FocusManager!.GetFocusedElement());
    }

    [AvaloniaFact]
    public void ReopeningReusesTheOverlayLayerWithoutLeakingChildren()
    {
        var grid = MakeGrid(out _);
        grid.EditorFactory = new TextBoxFactory();
        var before = grid.GetVisualChildren().Count();

        Assert.True(grid.BeginEdit(0, 0));
        var layer = grid.OverlayDecorator;
        var afterFirst = grid.GetVisualChildren().Count();
        Assert.Equal(before + 1, afterFirst);
        Assert.True(grid.EndEdit(commit: false));
        Assert.True(grid.BeginEdit(1, 1));
        Assert.True(grid.BeginEdit(2, 2));

        Assert.Same(layer, grid.OverlayDecorator);
        Assert.Equal(afterFirst, grid.GetVisualChildren().Count());
        Assert.Same(grid.ActiveEditor, layer.Child);
        Assert.True(grid.EndEdit(commit: false));
        Assert.Null(layer.Child);
        Assert.Equal(afterFirst, grid.GetVisualChildren().Count());
    }
}
