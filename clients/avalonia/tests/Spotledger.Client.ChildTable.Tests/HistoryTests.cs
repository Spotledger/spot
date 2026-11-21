using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>
/// The history seam. When the host gives a table the form's history, every undo key and the public
/// <see cref="SpotChildTable.Undo"/> and <see cref="SpotChildTable.Redo"/> act on that history, so the
/// table's own row journal is never replayed behind the form stack's back. With no history the table
/// is exactly the table it always was, and Ctrl+Z inside an open cell editor stays the editor's own.
/// </summary>
public sealed class HistoryTests
{
    private sealed class FakeHistory : IChildTableHistory
    {
        public bool CanUndo { get; set; } = true;

        public bool CanRedo { get; set; } = true;

        public int Undos { get; private set; }

        public int Redos { get; private set; }

        public ChildTableResult Answer { get; set; } = ChildTableResult.Ok((1, Fixtures.Desc), [1]);

        public ChildTableResult Undo()
        {
            Undos++;
            return Answer;
        }

        public ChildTableResult Redo()
        {
            Redos++;
            return Answer;
        }
    }

    private static SpotChildTable Make(out ChildTableModel model)
    {
        var draft = Fixtures.Draft();
        model = Fixtures.Model(draft);
        var table = new SpotChildTable(model);
        TestHost.Show(table);
        TestHost.Pump();
        return table;
    }

    private static string Desc(SpotChildTable table, int row) => table.Model.Text(row, Fixtures.Desc);

    [AvaloniaFact]
    public void WithAHistoryCtrlZUndoesTheFormsStepAndLeavesTheRowJournalAlone()
    {
        var table = Make(out var model);
        var history = new FakeHistory();
        table.History = history;
        Assert.True(model.SetCell(0, Fixtures.Desc, new TextFieldValue("edited")).Succeeded);
        var journal = model.Table.JournalCount;
        Assert.True(model.Table.CanUndo);

        var completed = new List<ChildTableOperationEventArgs>();
        table.OperationCompleted += (_, e) => completed.Add(e);

        Keys.Press(table, Key.Z, KeyModifiers.Control);

        Assert.Equal(1, history.Undos);
        Assert.Equal(0, history.Redos);
        // The table's own journal did not replay: the value is still the edited one.
        Assert.Equal("edited", Desc(table, 0));
        Assert.Equal(journal, model.Table.JournalCount);
        Assert.True(model.Table.CanUndo);

        // The key was marked handled and the outcome went out through OperationCompleted, as before.
        var raised = Assert.Single(completed);
        Assert.Equal("undo", raised.Operation);
        Assert.True(raised.Result.Succeeded);

        // The cursor and selection land where the history put them.
        Assert.Equal(1, table.Grid.GridCellCursor.RowIndex);
        Assert.Equal(Fixtures.Desc, table.Grid.GridCellCursor.ColumnIndex);
        Assert.Equal([1], table.Grid.SelectedRows);
    }

    [AvaloniaFact]
    public void WithAHistoryCtrlYAndCtrlShiftZRedoThroughIt()
    {
        var table = Make(out _);
        var history = new FakeHistory();
        table.History = history;

        Keys.Press(table, Key.Y, KeyModifiers.Control);
        Keys.Press(table, Key.Z, KeyModifiers.Control | KeyModifiers.Shift);

        Assert.Equal(0, history.Undos);
        Assert.Equal(2, history.Redos);
    }

    [AvaloniaFact]
    public void ThePublicUndoAndRedoGoThroughTheHistoryWhenItIsSet()
    {
        var table = Make(out var model);
        var history = new FakeHistory();
        table.History = history;
        Assert.True(model.SetCell(0, Fixtures.Desc, new TextFieldValue("edited")).Succeeded);

        Assert.True(table.Undo().Succeeded);
        Assert.True(table.Redo().Succeeded);

        Assert.Equal(1, history.Undos);
        Assert.Equal(1, history.Redos);
        Assert.Equal("edited", Desc(table, 0));
        Assert.True(model.Table.CanUndo);
    }

    [AvaloniaFact]
    public void WithNoHistoryTheTableUndoesAndRedoesItsOwnJournal()
    {
        var table = Make(out var model);
        Assert.Null(table.History);
        Assert.True(model.SetCell(0, Fixtures.Desc, new TextFieldValue("edited")).Succeeded);

        Assert.True(table.Undo().Succeeded);
        Assert.Equal("Line 1", Desc(table, 0));
        Assert.True(table.Redo().Succeeded);
        Assert.Equal("edited", Desc(table, 0));
    }

    [AvaloniaFact]
    public void CtrlZInsideAnOpenCellEditorStaysTheEditorsOwn()
    {
        var table = Make(out var model);
        var history = new FakeHistory();
        table.History = history;
        table.Grid.SetCursor(0, Fixtures.Desc);
        table.Grid.SetSelection([0]);
        table.Grid.Focus();
        TestHost.Pump();
        Keys.Press(table, Key.Enter);
        Assert.True(table.Grid.IsEditing);
        var journal = model.Table.JournalCount;

        Keys.Press(table, Key.Z, KeyModifiers.Control);

        Assert.Equal(0, history.Undos);
        Assert.Equal(0, history.Redos);
        Assert.Equal(journal, model.Table.JournalCount);
        Assert.True(table.Grid.IsEditing);
    }

    [AvaloniaFact]
    public void ARefusalFromTheHistoryIsReportedThroughOperationCompleted()
    {
        var table = Make(out var model);
        var history = new FakeHistory { Answer = ChildTableResult.Refused(ChildTableRefusal.NoWriteRight) };
        table.History = history;
        Assert.True(model.SetCell(0, Fixtures.Desc, new TextFieldValue("edited")).Succeeded);

        var completed = new List<ChildTableOperationEventArgs>();
        table.OperationCompleted += (_, e) => completed.Add(e);

        Keys.Press(table, Key.Z, KeyModifiers.Control);

        var raised = Assert.Single(completed);
        Assert.Equal("undo", raised.Operation);
        Assert.False(raised.Result.Succeeded);
        Assert.Equal(ChildTableRefusal.NoWriteRight, raised.Result.Refusal);
        Assert.Equal("edited", Desc(table, 0));
    }
}
