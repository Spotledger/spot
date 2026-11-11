using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// WP-UI-08.T6: the public cursor / selection / edit / scroll / key-hook surface, used the
/// way a separate project (the child-table control) would. Everything here goes through
/// <see cref="PublicSurface"/>, which touches public members only; reviewers check that no
/// internal member is referenced in this file.
/// </summary>
public sealed class PublicApiTests
{
    /// <summary>The consumer-side view of a grid: public members only.</summary>
    private sealed class PublicSurface(SpotGrid grid)
    {
        public List<GridCursorChangedEventArgs> CursorEvents { get; } = [];
        public int SelectionEvents { get; private set; }
        public List<Key> Keys { get; } = [];
        public bool ClaimKeys { get; set; }

        public void Attach()
        {
            grid.CursorChanged += (_, e) => CursorEvents.Add(e);
            grid.SelectionChanged += (_, _) => SelectionEvents++;
            grid.GridKeyDown += (_, e) =>
            {
                Keys.Add(e.Key);
                e.Handled = ClaimKeys;
            };
        }

        public GridCursor Cursor => grid.GridCellCursor;
        public IReadOnlyList<int> Selected => grid.SelectedRows;
        public bool Editing => grid.IsEditing;
        public (int Row, int Column)? EditingCell => grid.EditingCell;
        public int Rows => grid.RowCount;

        public void SetCursor(int row, int column) => grid.SetCursor(row, column);
        public void SetSelection(params int[] rows) => grid.SetSelection(rows);
        public bool End(bool commit) => grid.EndEdit(commit);
        public bool Begin(int row, int column) => grid.BeginEdit(row, column);
        public void ScrollTo(int row, int column = -1) => grid.ScrollIntoView(row, column);
        public bool TryRow(int row, out GridRow value) => grid.TryGetRow(row, out value);
    }

    private static SpotGrid MakeGrid(int rows = 200, GridSelectionMode mode = GridSelectionMode.Extended)
    {
        var source = new InMemoryGridSource(GridFixture.MakeRows(rows, 4));
        return GridFixtures.Make(source, mode)
            .With(source, GridFixtures.PixelColumns(4), width: 500, height: 400)
            .Build();
    }

    private static void Press(SpotGrid grid, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => grid.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent });

    private sealed class Factory : ICellEditorFactory
    {
        public int Commits { get; private set; }
        public int Cancels { get; private set; }
        public bool Reject { get; set; }
        public Control? CreateEditor(int rowIndex, int columnIndex, GridRow row, GridColumnModel column)
            => new TextBox { Text = row.Cells[columnIndex].Text };
        public string GetValue(Control editor) => ((TextBox)editor).Text ?? string.Empty;
        public bool TryCommit(int rowIndex, int columnIndex, string proposedValue, Control editor)
        {
            if (Reject)
            {
                return false;
            }

            Commits++;
            return true;
        }

        public void Cancel(int rowIndex, int columnIndex, Control editor) => Cancels++;
    }

    [AvaloniaFact]
    public void CursorStartsEmptyAndSetCursorClampsAndRaisesOnce()
    {
        var grid = MakeGrid();
        var api = new PublicSurface(grid);
        api.Attach();
        Assert.False(api.Cursor.HasValue);
        Assert.Equal(-1, api.Cursor.RowIndex);

        api.SetCursor(5, 2);
        var e = Assert.Single(api.CursorEvents);
        Assert.False(e.Old.HasValue);
        Assert.Equal(new GridCursor(5, 2, GridCursorOrigin.Keyboard), e.New);

        api.SetCursor(5, 2);
        Assert.Single(api.CursorEvents);

        api.SetCursor(9999, 99);
        Assert.Equal(199, api.Cursor.RowIndex);
        Assert.Equal(3, api.Cursor.ColumnIndex);
        Assert.Equal(2, api.CursorEvents.Count);
        Assert.False(api.Editing);
    }

    [AvaloniaFact]
    public void SelectionSetReadAndModeRules()
    {
        var grid = MakeGrid();
        var api = new PublicSurface(grid);
        api.Attach();
        api.SetSelection(7, 3, 3, 500, -1);
        Assert.Equal([3, 7], api.Selected);
        Assert.Equal(1, api.SelectionEvents);

        api.SetSelection(3, 7);
        Assert.Equal(1, api.SelectionEvents);

        var copy = api.Selected;
        api.SetSelection();
        Assert.Equal([3, 7], copy);
        Assert.Empty(api.Selected);
        Assert.Equal(2, api.SelectionEvents);

        var single = MakeGrid(mode: GridSelectionMode.OneRow);
        var one = new PublicSurface(single);
        one.SetSelection(9, 4, 6);
        Assert.Equal([9], one.Selected);
    }

    [AvaloniaFact]
    public void EditStateAndEndEdit()
    {
        var grid = MakeGrid();
        var factory = new Factory();
        grid.EditorFactory = factory;
        var api = new PublicSurface(grid);
        var finished = new List<bool>();
        grid.EditFinished += (_, e) => finished.Add(e.Committed);

        Assert.False(api.End(true));
        Assert.True(api.Begin(2, 1));
        Assert.True(api.Editing);
        Assert.Equal((2, 1), api.EditingCell);

        factory.Reject = true;
        Assert.False(api.End(true));
        Assert.True(api.Editing);

        factory.Reject = false;
        Assert.True(api.End(true));
        Assert.False(api.Editing);
        Assert.Null(api.EditingCell);
        Assert.Equal(1, factory.Commits);
        Assert.Equal([true], finished);

        Assert.True(api.Begin(1, 0));
        Assert.True(api.End(false));
        Assert.Equal(1, factory.Cancels);
    }

    [AvaloniaFact]
    public void ScrollAndRowAccess()
    {
        var grid = MakeGrid();
        var api = new PublicSurface(grid);
        Assert.Equal(200, api.Rows);
        Assert.True(api.TryRow(0, out var row));
        Assert.Equal("row 0", row.Cells[0].Text);
        Assert.False(api.TryRow(-1, out _));
        api.ScrollTo(150, 3);
        api.ScrollTo(100000);
        api.ScrollTo(-5);
    }

    [AvaloniaFact]
    public void KeyHookClaimsKeysAndLeavesOthers()
    {
        var grid = MakeGrid();
        var api = new PublicSurface(grid);
        api.Attach();
        api.ClaimKeys = true;
        Press(grid, Key.Down);
        Assert.False(api.Cursor.HasValue);
        Assert.Equal([Key.Down], api.Keys);

        api.ClaimKeys = false;
        Press(grid, Key.Down);
        Assert.True(api.Cursor.HasValue);
    }
}
