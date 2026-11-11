using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

/// <summary>
/// The gate the undo history enforces on every undo and redo. A step is checked for every table and
/// every header field it touches, before anything changes: the document state, the write right, a
/// masked or read-only field, and the form's own table gate. With no gate supplied nothing that
/// touches a child table runs (fail closed); a refusal changes nothing, leaves the step in place and
/// is returned to the caller.
/// </summary>
public sealed class UndoGateTests : IDisposable
{
    private readonly ManualClock _clock = new();
    private readonly DocumentDraft _draft = Models.LoadedInvoice();
    private readonly UndoStack _stack;
    private readonly ChildTableModel _model;

    public UndoGateTests()
    {
        _stack = new UndoStack(_draft, _clock, schedule: _ => { });
        _model = new ChildTableModel(_draft, "items") { CanWrite = true };
        _stack.SetGate(name => string.Equals(name, "items", StringComparison.Ordinal) ? _model.CheckEditable() : null);
    }

    public void Dispose()
    {
        _stack.Dispose();
        _model.Dispose();
    }

    private string Title => ((TextFieldValue)_draft.Get("title")!).Value;

    private string Ids() => string.Join(',', _draft.Tables["items"].Rows.Select(row => row.Id ?? "-"));

    private void AddRow()
    {
        Assert.True(_model.AddRow().Succeeded);
        _stack.Flush();
    }

    private void Type(string field, string text)
    {
        _draft.Set(field, new TextFieldValue(text));
        _stack.Flush();
    }

    private static void ReadOnly(DocumentDraft draft, string field)
        => draft.ApplyPermissions(PermissionSnapshot.Decode(
            """{"etag":"e2","data":{"doctypes":[{"doctype":"invoice","rights":{"create":true,"write":true},"fields":{"read_only":["FIELD"]}}]}}"""
                .Replace("FIELD", field, StringComparison.Ordinal)));

    [Fact]
    public void A_row_step_is_refused_when_the_form_supplied_no_gate()
    {
        var draft = Models.LoadedInvoice();
        using var stack = new UndoStack(draft, _clock, schedule: _ => { });
        draft.Tables["items"].AddRow();
        stack.Flush();
        var ids = string.Join(',', draft.Tables["items"].Rows.Select(row => row.Id ?? "-"));

        var result = stack.TryUndo();

        Assert.False(result.Succeeded);
        Assert.Equal(ChildTableRefusal.NoWriteRight, result.Refusal);
        Assert.False(string.IsNullOrEmpty(result.Message));
        Assert.Equal(ids, string.Join(',', draft.Tables["items"].Rows.Select(row => row.Id ?? "-")));
        Assert.True(stack.CanUndo);
        Assert.False(stack.Undo());
    }

    [Fact]
    public void A_header_only_step_needs_no_gate_because_the_default_field_gate_reads_the_draft()
    {
        var draft = Models.LoadedInvoice();
        using var stack = new UndoStack(draft, _clock, schedule: _ => { });
        draft.Set("title", new TextFieldValue("Changed"));
        stack.Flush();

        Assert.True(stack.TryUndo().Succeeded);
        Assert.Equal("October", ((TextFieldValue)draft.Get("title")!).Value);
    }

    [Fact]
    public void A_row_step_runs_and_reverses_when_the_host_gate_allows_it()
    {
        var before = Ids();
        AddRow();
        Assert.Equal(before + ",-", Ids());

        var result = _stack.TryUndo();

        Assert.True(result.Succeeded);
        Assert.Equal(before, Ids());
        Assert.False(_stack.CanUndo);
        Assert.True(_stack.CanRedo);
        Assert.Null(_stack.Note);
    }

    [Fact]
    public void A_refused_row_step_leaves_the_step_and_the_data_exactly_where_they_were()
    {
        AddRow();
        var ids = Ids();
        _model.CanWrite = false;

        var result = _stack.TryUndo();

        Assert.False(result.Succeeded);
        Assert.Equal(ChildTableRefusal.NoWriteRight, result.Refusal);
        Assert.Equal(ids, Ids());
        Assert.True(_stack.CanUndo);
        Assert.Equal("Add row", _stack.UndoLabel);
        Assert.False(_stack.CanRedo);
    }

    [Theory]
    [InlineData("posted", ChildTableRefusal.DocumentPosted)]
    [InlineData("cancelled", ChildTableRefusal.DocumentCancelled)]
    [InlineData("submitted-for-audit", ChildTableRefusal.DocumentStateUnknown)]
    public void Every_step_is_refused_when_the_document_is_not_a_draft(string state, ChildTableRefusal expected)
    {
        // The document arrives in a state that closes it. The host gate is open, so only the
        // document rule can be what refuses.
        _stack.SetGate(_ => null);
        _draft.LoadFrom($$"""{"_state":"{{state}}"}""");

        Type("title", "Changed");
        var header = _stack.TryUndo();
        Assert.False(header.Succeeded);
        Assert.Equal(expected, header.Refusal);
        Assert.Equal("Changed", Title);
        Assert.True(_stack.CanUndo);

        _draft.Tables["items"].AddRow();
        _stack.Flush();
        var ids = Ids();
        var row = _stack.TryUndo();
        Assert.False(row.Succeeded);
        Assert.Equal(expected, row.Refusal);
        Assert.Equal(ids, Ids());
    }

    [Fact]
    public void A_masked_field_refuses_undo_and_redo()
    {
        Type("ssn", "999");
        Models.Mask(_draft, "ssn");

        var undo = _stack.TryUndo();
        Assert.False(undo.Succeeded);
        Assert.Equal(ChildTableRefusal.CellMasked, undo.Refusal);
        Assert.True(_draft.Fields["ssn"].IsMasked);
        Assert.True(_stack.CanUndo);

        var redraft = Models.LoadedInvoice();
        using var stack = new UndoStack(redraft, _clock, schedule: _ => { });
        redraft.Set("ssn", new TextFieldValue("999"));
        stack.Flush();
        Assert.True(stack.TryUndo().Succeeded);
        Models.Mask(redraft, "ssn");

        var redo = stack.TryRedo();
        Assert.False(redo.Succeeded);
        Assert.Equal(ChildTableRefusal.CellMasked, redo.Refusal);
        Assert.True(stack.CanRedo);
        Assert.True(redraft.Fields["ssn"].IsMasked);
    }

    [Fact]
    public void A_read_only_field_refuses_undo_and_redo()
    {
        ReadOnly(_draft, "title");
        Type("title", "Changed");

        var undo = _stack.TryUndo();
        Assert.False(undo.Succeeded);
        Assert.Equal(ChildTableRefusal.CellReadOnly, undo.Refusal);
        Assert.Equal("Changed", Title);

        var redraft = Models.LoadedInvoice();
        using var stack = new UndoStack(redraft, _clock, schedule: _ => { });
        redraft.Set("title", new TextFieldValue("Changed"));
        stack.Flush();
        Assert.True(stack.TryUndo().Succeeded);
        ReadOnly(redraft, "title");

        var redo = stack.TryRedo();
        Assert.False(redo.Succeeded);
        Assert.Equal(ChildTableRefusal.CellReadOnly, redo.Refusal);
        Assert.Equal("October", ((TextFieldValue)redraft.Get("title")!).Value);
    }

    [Fact]
    public void An_undo_that_becomes_allowed_again_then_works()
    {
        AddRow();
        _model.CanWrite = false;
        Assert.False(_stack.TryUndo().Succeeded);
        Assert.True(_stack.CanUndo);

        _model.CanWrite = true;
        Assert.True(_stack.TryUndo().Succeeded);
        Assert.False(_stack.CanUndo);
    }

    [Fact]
    public void A_redo_that_becomes_allowed_again_then_works()
    {
        var before = Ids();
        AddRow();
        Assert.True(_stack.TryUndo().Succeeded);
        _model.CanWrite = false;
        Assert.False(_stack.TryRedo().Succeeded);
        Assert.True(_stack.CanRedo);

        _model.CanWrite = true;
        Assert.True(_stack.TryRedo().Succeeded);
        Assert.Equal(before + ",-", Ids());
    }

    [Fact]
    public void Nothing_to_undo_or_redo_is_reported_as_nothing_to_do()
    {
        var nothing = _stack.TryUndo();
        Assert.False(nothing.Succeeded);
        Assert.Equal(ChildTableRefusal.NothingToDo, nothing.Refusal);

        var noRedo = _stack.TryRedo();
        Assert.False(noRedo.Succeeded);
        Assert.Equal(ChildTableRefusal.NothingToDo, noRedo.Refusal);
    }

    [Fact]
    public void The_cursor_and_selection_name_the_row_the_row_step_touched()
    {
        AddRow();
        Assert.True(_model.DeleteRows([1]).Succeeded);
        _stack.Flush();
        var adapter = new ChildTableHistoryAdapter(_stack, "items");

        var result = adapter.Undo();

        Assert.True(result.Succeeded);
        Assert.True(result.Cursor.HasValue);
        var cursor = result.Cursor!.Value;
        Assert.Equal(1, cursor.Row);
        Assert.Equal(-1, cursor.Column);
        Assert.Equal(new[] { 1 }, result.Selection!);
    }

    [Fact]
    public void A_header_step_names_no_row_so_the_table_keeps_its_cursor()
    {
        Type("title", "Changed");
        var adapter = new ChildTableHistoryAdapter(_stack, "items");

        var result = adapter.Undo();

        Assert.True(result.Succeeded);
        Assert.Null(result.Cursor);
        Assert.Null(result.Selection);
    }

    [Fact]
    public void The_stack_and_the_tables_journal_never_go_out_of_step()
    {
        var adapter = new ChildTableHistoryAdapter(_stack, "items");
        var table = _draft.Tables["items"];
        var original = Ids();

        for (var i = 0; i < 30; i++)
        {
            _clock.Advance(TimeSpan.FromSeconds(2));
            switch (i % 6)
            {
                case 0:
                    Assert.True(_model.AddRow().Succeeded);
                    break;
                case 1:
                    Assert.True(_model.SetCell(0, 0, new TextFieldValue("v" + i)).Succeeded);
                    break;
                case 2:
                    Assert.True(_model.InsertRow(1).Succeeded);
                    break;
                case 3:
                    Assert.True(_model.DeleteRows([table.Rows.Count - 1]).Succeeded);
                    break;
                case 4:
                    Assert.True(adapter.Undo().Succeeded);
                    break;
                default:
                    Assert.True(adapter.Redo().Succeeded);
                    break;
            }

            _stack.Flush();
            Assert.Equal(table.JournalCount, _stack.Count);
            Assert.Equal(table.CanUndo, _stack.CanUndo);
            Assert.Equal(table.CanRedo, _stack.CanRedo);
        }

        Assert.Null(_stack.Note);
        Assert.True(_stack.Count > 0);

        while (_stack.CanUndo)
        {
            Assert.True(adapter.Undo().Succeeded);
        }

        Assert.False(table.CanUndo);
        Assert.Equal(original, Ids());

        while (_stack.CanRedo)
        {
            Assert.True(adapter.Redo().Succeeded);
        }

        Assert.False(table.CanRedo);
    }

    [Fact]
    public async Task A_draft_restore_against_a_posted_document_is_refused_by_the_rule_the_stack_uses()
    {
        var edited = Models.LoadedInvoice();
        edited.Set("title", new TextFieldValue("My edit"));
        var record = DraftSnapshot.Capture(edited, DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow());
        var store = new InMemoryDraftStore();
        _ = await store.SaveAsync(record);

        var draft = Models.LoadedInvoice();
        draft.LoadFrom("""{"_state":"posted"}""");
        Assert.True(UndoGate.IsDocumentReadOnly(draft));

        using var stack = new UndoStack(draft, _clock, schedule: _ => { });
        draft.Set("title", new TextFieldValue("posted edit"));
        stack.Flush();
        Assert.False(stack.TryUndo().Succeeded);

        var bar = new RecoveryBarViewModel(store, record, draft, isDocumentReadOnly: () => UndoGate.IsDocumentReadOnly(draft));
        bar.RestoreCommand.Execute(null);
        await bar.RestoreCommand.ExecutionTask;

        Assert.Equal("This document is read-only, so nothing was restored.", bar.Result);
        Assert.Equal("posted edit", ((TextFieldValue)draft.Get("title")!).Value);
    }
}
