using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

/// <summary>
/// The adapter a child table is given: it hands the table's undo keys to the form's one stack and
/// reports the row the step touched, so the table's cursor follows an undo made anywhere in the form.
/// It adds no checks of its own: the stack's gate decides, and a refusal comes back unchanged.
/// </summary>
public sealed class ChildTableHistoryAdapterTests : IDisposable
{
    private readonly ManualClock _clock = new();
    private readonly DocumentDraft _draft = Models.LoadedInvoice();
    private readonly UndoStack _stack;
    private readonly ChildTableHistoryAdapter _adapter;

    public ChildTableHistoryAdapterTests()
    {
        _stack = new UndoStack(_draft, _clock, schedule: _ => { });
        _adapter = new ChildTableHistoryAdapter(_stack, "items");
    }

    public void Dispose() => _stack.Dispose();

    private string Ids() => string.Join(',', _draft.Tables["items"].Rows.Select(row => row.Id ?? "-"));

    [Fact]
    public void Undo_and_redo_go_through_the_stack()
    {
        _stack.SetGate(_ => null);
        var before = Ids();
        _draft.Tables["items"].AddRow();
        _stack.Flush();
        Assert.True(_adapter.CanUndo);
        Assert.False(_adapter.CanRedo);

        var undo = _adapter.Undo();

        Assert.True(undo.Succeeded);
        Assert.Equal(before, Ids());
        Assert.False(_adapter.CanUndo);
        Assert.True(_adapter.CanRedo);

        var redo = _adapter.Redo();

        Assert.True(redo.Succeeded);
        Assert.Equal(before + ",-", Ids());
        Assert.True(_adapter.CanUndo);
        Assert.False(_adapter.CanRedo);
    }

    [Fact]
    public void The_adapter_adds_no_checks_of_its_own_so_the_default_gate_still_refuses()
    {
        var before = Ids();
        _draft.Tables["items"].AddRow();
        _stack.Flush();

        var result = _adapter.Undo();

        Assert.False(result.Succeeded);
        Assert.Equal(ChildTableRefusal.NoWriteRight, result.Refusal);
        Assert.Equal(before + ",-", Ids());
        Assert.True(_adapter.CanUndo);
    }

    [Fact]
    public void A_refusal_comes_back_unchanged_and_names_no_row()
    {
        _stack.SetGate(_ => ChildTableResult.Refused(ChildTableRefusal.TableMasked));
        _draft.Tables["items"].AddRow();
        _stack.Flush();

        var result = _adapter.Undo();

        Assert.False(result.Succeeded);
        Assert.Equal(ChildTableRefusal.TableMasked, result.Refusal);
        Assert.Null(result.Cursor);
    }

    [Fact]
    public void A_step_on_another_table_does_not_move_this_tables_cursor()
    {
        var draft = TwoTables();
        using var stack = new UndoStack(draft, _clock, schedule: _ => { });
        stack.SetGate(_ => null);
        var items = new ChildTableHistoryAdapter(stack, "items");
        draft.Tables["taxes"].Rows[0].SetValue("code", new TextFieldValue("GST"));
        stack.Flush();

        var result = items.Undo();

        Assert.True(result.Succeeded);
        Assert.Equal("VAT", ((TextFieldValue)draft.Tables["taxes"].Rows[0].GetValue("code")!).Value);
        Assert.Null(result.Cursor);
        Assert.Null(result.Selection);
    }

    [Fact]
    public void A_step_on_this_table_names_the_row_it_landed_on()
    {
        _stack.SetGate(_ => null);
        _draft.Tables["items"].Rows[0].SetValue("item", new TextFieldValue("Washer"));
        _stack.Flush();

        var result = _adapter.Undo();

        Assert.True(result.Succeeded);
        Assert.True(result.Cursor.HasValue);
        var cursor = result.Cursor!.Value;
        Assert.Equal(0, cursor.Row);
        // -1 keeps the column the cursor already had: an undo moves rows, not columns.
        Assert.Equal(-1, cursor.Column);
        Assert.Equal(new[] { 0 }, result.Selection!);
    }

    private static DocumentDraft TwoTables()
    {
        var line = new DocTypeModel("line", "Line", 1, false, [new FieldModel("item", "Item", FieldKind.Text)]);
        var tax = new DocTypeModel("tax", "Tax", 1, false, [new FieldModel("code", "Code", FieldKind.Text)]);
        var model = new DocTypeModel("invoice", "Invoice", 1, true,
        [
            new FieldModel("title", "Title", FieldKind.Text),
            new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line"),
            new FieldModel("taxes", "Taxes", FieldKind.Table, childDoctype: "tax"),
        ],
        new Dictionary<string, DocTypeModel> { ["items"] = line, ["taxes"] = tax });
        var draft = new DocumentDraft(model);
        draft.LoadFrom("""{"id":"INV-2","version":1,"_state":"draft","items":[{"id":"i1","item":"A"}],"taxes":[{"id":"t1","code":"VAT"}]}""");
        return draft;
    }
}
