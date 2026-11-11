using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Interactivity;
using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

/// <summary>
/// The bug this task fixes, end to end: a Ctrl+Z pressed inside a child table must undo the
/// <b>form's</b> last step — a header edit, a row add or a row edit — through the table's history
/// adapter, and the form stack must stay in step with the table's own row journal (it is the stack
/// that replays the journal, and the table's key path must not replay it a second time).
/// </summary>
public sealed class ChildTableUndoRoutingTests
{
    private sealed class Rig : IDisposable
    {
        public Rig()
        {
            Clock = new ManualClock();
            Draft = Models.LoadedInvoice();
            Stack = new UndoStack(Draft, Clock, schedule: _ => { });
            Model = new ChildTableModel(Draft, "items") { CanWrite = true };
            Stack.SetGate(name => string.Equals(name, "items", StringComparison.Ordinal) ? Model.CheckEditable() : null);
            Table = new SpotChildTable(Model) { History = new ChildTableHistoryAdapter(Stack, "items") };
            TestHost.Show(Table);
            TestHost.Pump();
            Table.Grid.Focus();
            TestHost.Pump();
        }

        public ManualClock Clock { get; }

        public DocumentDraft Draft { get; }

        public UndoStack Stack { get; }

        public ChildTableModel Model { get; }

        public SpotChildTable Table { get; }

        public void Press(Key key, KeyModifiers modifiers = KeyModifiers.None)
        {
            Table.Grid.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent });
            TestHost.Pump();
        }

        public void Dispose()
        {
            Table.Dispose();
            Stack.Dispose();
        }
    }

    private static string Title(DocumentDraft draft) => ((TextFieldValue)draft.Get("title")!).Value;

    private static string ItemAt(DocumentDraft draft, int row) => ((TextFieldValue)draft.Tables["items"].Rows[row].GetValue("item")!).Value;

    [AvaloniaFact]
    public void Ctrl_z_in_the_grid_undoes_the_forms_last_step_three_times_in_reverse_order()
    {
        using var rig = new Rig();
        rig.Draft.Set("title", new TextFieldValue("Changed"));
        rig.Stack.Flush();
        Assert.True(rig.Model.AddRow().Succeeded);
        rig.Stack.Flush();
        Assert.True(rig.Model.SetCell(0, 0, new TextFieldValue("Washer")).Succeeded);
        rig.Stack.Flush();
        Assert.Equal(3, rig.Stack.Count);

        rig.Press(Key.Z, KeyModifiers.Control);
        Assert.Equal("Bolt", ItemAt(rig.Draft, 0));
        Assert.Equal(3, rig.Draft.Tables["items"].Rows.Count);

        rig.Press(Key.Z, KeyModifiers.Control);
        Assert.Equal(2, rig.Draft.Tables["items"].Rows.Count);

        rig.Press(Key.Z, KeyModifiers.Control);
        Assert.Equal("October", Title(rig.Draft));

        Assert.False(rig.Stack.CanUndo);
        Assert.True(rig.Stack.CanRedo);
        // The stack never went out of step and cleared itself.
        Assert.Null(rig.Stack.Note);
        Assert.Equal(3, rig.Stack.Count);
    }

    [AvaloniaFact]
    public void Ctrl_y_and_ctrl_shift_z_redo_the_steps_through_the_same_history()
    {
        using var rig = new Rig();
        rig.Draft.Set("title", new TextFieldValue("Changed"));
        rig.Stack.Flush();
        Assert.True(rig.Model.AddRow().Succeeded);
        rig.Stack.Flush();
        rig.Press(Key.Z, KeyModifiers.Control);
        rig.Press(Key.Z, KeyModifiers.Control);
        Assert.Equal(2, rig.Draft.Tables["items"].Rows.Count);
        Assert.Equal("October", Title(rig.Draft));

        rig.Press(Key.Y, KeyModifiers.Control);
        Assert.Equal("Changed", Title(rig.Draft));
        rig.Press(Key.Z, KeyModifiers.Control | KeyModifiers.Shift);
        Assert.Equal(3, rig.Draft.Tables["items"].Rows.Count);
        Assert.False(rig.Stack.CanRedo);
        Assert.Null(rig.Stack.Note);
    }

    [AvaloniaFact]
    public void The_cursor_lands_on_the_row_a_routed_undo_touched()
    {
        using var rig = new Rig();
        Assert.True(rig.Model.DeleteRows([1]).Succeeded);
        rig.Stack.Flush();
        Assert.Single(rig.Draft.Tables["items"].Rows);

        rig.Press(Key.Z, KeyModifiers.Control);

        Assert.Equal(2, rig.Draft.Tables["items"].Rows.Count);
        Assert.Equal(1, rig.Table.Grid.GridCellCursor.RowIndex);
        Assert.Equal(new[] { 1 }, rig.Table.Grid.SelectedRows);
    }

    [AvaloniaFact]
    public void A_refused_routed_undo_changes_nothing_and_reports_through_operation_completed()
    {
        using var rig = new Rig();
        Assert.True(rig.Model.AddRow().Succeeded);
        rig.Stack.Flush();
        rig.Model.CanWrite = false;
        var completed = new List<ChildTableOperationEventArgs>();
        rig.Table.OperationCompleted += (_, e) => completed.Add(e);

        rig.Press(Key.Z, KeyModifiers.Control);

        Assert.Equal(3, rig.Draft.Tables["items"].Rows.Count);
        Assert.True(rig.Stack.CanUndo);
        var raised = Assert.Single(completed);
        Assert.Equal("undo", raised.Operation);
        Assert.Equal(ChildTableRefusal.NoWriteRight, raised.Result.Refusal);
    }
}
