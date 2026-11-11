using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

public sealed class UndoStackTests : IDisposable
{
    private readonly ManualClock _clock = new();
    private readonly DocumentDraft _draft = Models.LoadedInvoice();
    private readonly UndoStack _stack;

    public UndoStackTests() => _stack = new UndoStack(_draft, _clock, schedule: _ => { });

    public void Dispose() => _stack.Dispose();

    private string Title => ((TextFieldValue)_draft.Get("title")!).Value;

    private void Type(string field, string text)
    {
        _draft.Set(field, new TextFieldValue(text));
        _stack.Flush();
    }

    private string Ids() => string.Join(',', _draft.Tables["items"].Rows.Select(r => r.Id ?? "-"));

    private string[] RowItems() => [.. _draft.Tables["items"].Rows.Select(r => ((TextFieldValue)r.GetValue("item")!).Value)];

    [Fact]
    public void A_field_edit_is_undone_and_redone_with_its_label()
    {
        Type("title", "Changed");

        Assert.True(_stack.CanUndo);
        Assert.False(_stack.CanRedo);
        Assert.Equal("Edit Title", _stack.UndoLabel);
        Assert.True(_stack.Undo());
        Assert.Equal("October", Title);
        Assert.False(_draft.IsDirty);
        Assert.Equal("Edit Title", _stack.RedoLabel);
        Assert.True(_stack.Redo());
        Assert.Equal("Changed", Title);
        Assert.False(_stack.Redo());
    }

    [Fact]
    public void Typing_in_one_field_within_a_second_is_one_step_and_a_pause_starts_another()
    {
        Type("title", "C");
        _clock.Advance(TimeSpan.FromMilliseconds(400));
        Type("title", "Ch");
        _clock.Advance(TimeSpan.FromMilliseconds(900));
        Type("title", "Cha");
        Assert.Equal(1, _stack.Count);

        _clock.Advance(TimeSpan.FromSeconds(1.5));
        Type("title", "Chan");
        Assert.Equal(2, _stack.Count);

        _ = _stack.Undo();
        Assert.Equal("Cha", Title);
        _ = _stack.Undo();
        Assert.Equal("October", Title);
    }

    [Fact]
    public void Coalescing_does_not_cross_fields()
    {
        Type("title", "A");
        Type("ssn", "n");
        Assert.Equal(2, _stack.Count);

        _clock.Advance(TimeSpan.FromSeconds(3));
        Type("title", "B");
        Type("title", "October");
        Assert.Equal(3, _stack.Count);
        Assert.Equal("Edit Title", _stack.UndoLabel);

        var fresh = new UndoStack(Models.LoadedInvoice(), _clock, schedule: _ => { });
        Assert.False(fresh.CanUndo);
        Assert.Null(fresh.UndoLabel);
    }

    [Fact]
    public void Typing_back_to_the_starting_text_within_the_window_leaves_no_step()
    {
        Type("title", "X");
        Type("title", "October");

        Assert.False(_stack.CanUndo);
        Assert.Equal(0, _stack.Count);
    }

    [Fact]
    public void A_new_edit_after_an_undo_drops_the_redo_steps()
    {
        Type("title", "One");
        _clock.Advance(TimeSpan.FromSeconds(5));
        Type("title", "Two");
        _ = _stack.Undo();
        Assert.True(_stack.CanRedo);

        _clock.Advance(TimeSpan.FromSeconds(5));
        Type("title", "Three");

        Assert.False(_stack.CanRedo);
        Assert.Equal(2, _stack.Count);
        _ = _stack.Undo();
        Assert.Equal("One", Title);
    }

    [Fact]
    public void A_derived_change_made_by_an_instant_rule_is_undone_with_the_edit_that_caused_it()
    {
        _draft.FieldChanged += (_, path) =>
        {
            if (path == "qty" && _draft.Get("qty") is DecimalFieldValue q)
            {
                _draft.Set("amount", new DecimalFieldValue(q.ExactValue.ToDecimalValue() * 10m));
            }
        };

        _draft.Set("qty", new DecimalFieldValue(4m));
        _stack.Flush();

        Assert.Equal(1, _stack.Count);
        Assert.Equal(40m, ((DecimalFieldValue)_draft.Get("amount")!).ExactValue.ToDecimalValue());
        _ = _stack.Undo();
        Assert.Equal(1m, ((DecimalFieldValue)_draft.Get("qty")!).ExactValue.ToDecimalValue());
        Assert.Equal(10m, ((DecimalFieldValue)_draft.Get("amount")!).ExactValue.ToDecimalValue());
        _ = _stack.Redo();
        Assert.Equal(4m, ((DecimalFieldValue)_draft.Get("qty")!).ExactValue.ToDecimalValue());
        Assert.Equal(40m, ((DecimalFieldValue)_draft.Get("amount")!).ExactValue.ToDecimalValue());
    }

    [Fact]
    public void A_form_event_diff_applied_later_is_folded_into_the_step_that_caused_it()
    {
        _draft.Set("qty", new DecimalFieldValue(4m));
        _stack.Flush();

        _stack.RunDerived(() =>
        {
            _draft.Set("amount", new DecimalFieldValue(44m));
            _draft.Set("ssn", new TextFieldValue("server-set"));
        });

        Assert.Equal(1, _stack.Count);
        _ = _stack.Undo();
        Assert.Equal(10m, ((DecimalFieldValue)_draft.Get("amount")!).ExactValue.ToDecimalValue());
        Assert.Equal("123-45", ((TextFieldValue)_draft.Get("ssn")!).Value);
        _ = _stack.Redo();
        Assert.Equal(44m, ((DecimalFieldValue)_draft.Get("amount")!).ExactValue.ToDecimalValue());
        Assert.Equal("server-set", ((TextFieldValue)_draft.Get("ssn")!).Value);
    }

    [Fact]
    public void Child_row_add_insert_delete_and_move_are_undone_with_ids_and_positions()
    {
        UndoTestGates.Allow(_stack);
        var table = _draft.Tables["items"];
        _ = table.AddRow();
        _stack.Flush();
        Assert.Equal("Add row", _stack.UndoLabel);
        _ = table.AddRowAt(0);
        _stack.Flush();
        Assert.Equal("Insert row", _stack.UndoLabel);
        table.RemoveRowAt(1);
        _stack.Flush();
        Assert.Equal("Delete row", _stack.UndoLabel);
        table.MoveRow(0, 2);
        _stack.Flush();
        Assert.Equal("Move row", _stack.UndoLabel);
        Assert.Equal(4, _stack.Count);
        Assert.Equal("r2,-,-", Ids());

        _ = _stack.Undo();
        Assert.Equal("-,r2,-", Ids());
        _ = _stack.Undo();
        Assert.Equal("-,r1,r2,-", Ids());
        _ = _stack.Undo();
        Assert.Equal("r1,r2,-", Ids());
        _ = _stack.Undo();
        Assert.Equal("r1,r2", Ids());
        Assert.False(_draft.IsDirty);

        while (_stack.Redo())
        {
        }

        Assert.Equal("r2,-,-", Ids());
    }

    [Fact]
    public void A_deleted_row_comes_back_with_its_id_and_values()
    {
        UndoTestGates.Allow(_stack);
        var table = _draft.Tables["items"];
        table.RemoveRowAt(0);
        _stack.Flush();
        Assert.Single(table.Rows);

        _ = _stack.Undo();

        Assert.Equal("r1,r2", Ids());
        Assert.Equal("Bolt", ((TextFieldValue)table.Rows[0].GetValue("item")!).Value);
        Assert.False(_draft.IsDirty);
    }

    [Fact]
    public void A_cell_edit_is_a_step_named_for_the_column()
    {
        UndoTestGates.Allow(_stack);
        _draft.Tables["items"].Rows[1].SetValue("item", new TextFieldValue("Screw"));
        _stack.Flush();

        Assert.Equal("Edit Item", _stack.UndoLabel);
        _ = _stack.Undo();
        Assert.Equal(["Bolt", "Nut"], RowItems());
    }

    [Fact]
    public void A_paste_or_bulk_fill_is_one_step()
    {
        UndoTestGates.Allow(_stack);
        var table = _draft.Tables["items"];
        using (var operation = table.Begin("paste 3 rows"))
        {
            for (var i = 0; i < 3; i++)
            {
                table.AddRow().SetValue("item", new TextFieldValue("P" + i));
            }

            operation.Commit();
        }

        _stack.Flush();
        Assert.Equal(1, _stack.Count);
        Assert.Equal("Paste 3 rows", _stack.UndoLabel);
        Assert.Equal(5, table.Rows.Count);

        using (var fill = table.Begin("fill down"))
        {
            foreach (var row in table.Rows)
            {
                row.SetValue("item", new TextFieldValue("same"));
            }

            fill.Commit();
        }

        _stack.Flush();
        Assert.Equal(2, _stack.Count);
        _ = _stack.Undo();
        Assert.Equal(["Bolt", "Nut", "P0", "P1", "P2"], RowItems());
        _ = _stack.Undo();
        Assert.Equal(["Bolt", "Nut"], RowItems());
        _ = _stack.Redo();
        _ = _stack.Redo();
        Assert.Equal(["same", "same", "same", "same", "same"], RowItems());
    }

    [Fact]
    public void A_row_edit_and_its_derived_field_change_are_one_step()
    {
        UndoTestGates.Allow(_stack);
        _draft.FieldChanged += (_, path) =>
        {
            if (path.StartsWith("items[", StringComparison.Ordinal))
            {
                _draft.Set("title", new TextFieldValue("rows changed"));
            }
        };

        _draft.Tables["items"].Rows[0].SetValue("item", new TextFieldValue("Washer"));
        _stack.Flush();

        Assert.Equal(1, _stack.Count);
        _ = _stack.Undo();
        Assert.Equal("October", Title);
        Assert.Equal(["Bolt", "Nut"], RowItems());
        _ = _stack.Redo();
        Assert.Equal("rows changed", Title);
        Assert.Equal(["Washer", "Nut"], RowItems());
    }

    [Fact]
    public void A_save_a_reload_and_a_revert_clear_the_history_with_a_visible_note()
    {
        var notes = new List<string>();
        _stack.Cleared += (_, note) => notes.Add(note);

        Type("title", "x");
        _draft.MarkSaved();
        Assert.False(_stack.CanUndo);
        Assert.Equal("Undo history cleared after save.", _stack.Note);

        Type("title", "y");
        _draft.LoadFrom("""{"id":"INV-1","version":4,"title":"fresh"}""");
        Assert.False(_stack.CanUndo);
        Assert.Contains("reloaded", _stack.Note);

        Type("title", "z");
        _draft.Revert();
        Assert.False(_stack.CanUndo);
        Assert.Contains("reverted", _stack.Note);
        Assert.Equal(3, notes.Count);
    }

    [Fact]
    public void Undo_after_a_save_cannot_bring_back_pre_save_edits()
    {
        Type("title", "x");
        _draft.MarkSaved();
        Assert.False(_stack.Undo());
        Assert.Equal("x", Title);
    }

    [Fact]
    public void The_history_is_bounded()
    {
        using var small = new UndoStack(_draft, _clock, bound: 5, schedule: _ => { });
        for (var i = 0; i < 20; i++)
        {
            _clock.Advance(TimeSpan.FromSeconds(2));
            _draft.Set("title", new TextFieldValue("v" + i));
            small.Flush();
        }

        Assert.Equal(5, small.Count);
        for (var i = 0; i < 5; i++)
        {
            Assert.True(small.Undo());
        }

        Assert.False(small.Undo());
        Assert.Equal("v14", Title);
    }

    [Fact]
    public void Ten_thousand_edits_keep_the_history_and_the_row_journal_flat()
    {
        UndoTestGates.Allow(_stack);
        var table = _draft.Tables["items"];
        for (var i = 0; i < 10_000; i++)
        {
            _clock.Advance(TimeSpan.FromSeconds(2));
            if (i % 2 == 0)
            {
                _draft.Set("title", new TextFieldValue("v" + i));
            }
            else
            {
                table.Rows[0].SetValue("item", new TextFieldValue("i" + i));
            }

            _stack.Flush();
        }

        Assert.Equal(UndoStack.DefaultBound, _stack.Count);
        Assert.True(table.JournalCount <= table.JournalDepth);
        Assert.True(table.JournalDepth <= UndoStack.DefaultBound * 8);
        for (var i = 0; i < UndoStack.DefaultBound; i++)
        {
            Assert.True(_stack.Undo());
        }

        Assert.False(_stack.Undo());
    }

    [Fact]
    public void A_table_undone_by_its_own_control_clears_the_history_instead_of_guessing()
    {
        _draft.Tables["items"].Rows[0].SetValue("item", new TextFieldValue("Washer"));
        _stack.Flush();

        _ = _draft.Tables["items"].UndoRow();

        Assert.False(_stack.CanUndo);
        Assert.Contains("own control", _stack.Note);
    }

    [Fact]
    public void Undo_never_writes_a_masked_field_back()
    {
        Type("ssn", "999");
        Models.Mask(_draft, "ssn");
        _ = _stack.Undo();
        Assert.True(_draft.Fields["ssn"].IsMasked);
        Assert.NotEqual("123-45", (_draft.Fields["ssn"].Value as TextFieldValue)?.Value);
    }

    [Fact]
    public void The_scheduler_closes_the_edit_at_the_end_of_the_turn_so_separate_edits_are_separate_steps()
    {
        var queue = new Queue<Action>();
        using var scheduled = new UndoStack(_draft, _clock, schedule: queue.Enqueue);
        _draft.Set("title", new TextFieldValue("a"));
        _draft.Set("amount", new DecimalFieldValue(1m));
        Assert.Single(queue);
        queue.Dequeue()();
        Assert.Equal(1, scheduled.Count);

        _draft.Set("ssn", new TextFieldValue("n"));
        queue.Dequeue()();
        Assert.Equal(2, scheduled.Count);
    }
}

/// <summary>
/// The gate a host supplies. A history that undoes or redoes a row step needs one, because a step
/// that touches a child table is refused by default (fail closed); tests whose steps are header
/// edits only need no gate at all, because the default field gate reads the draft's field state.
/// </summary>
internal static class UndoTestGates
{
    /// <summary>Tells the stack that every table may change now.</summary>
    public static void Allow(UndoStack stack) => stack.SetGate(_ => null);
}

internal static class DecimalTestExtensions
{
    public static decimal ToDecimalValue(this ExactDecimal value) => decimal.Parse(value.ToCanonicalText(), System.Globalization.CultureInfo.InvariantCulture);
}
