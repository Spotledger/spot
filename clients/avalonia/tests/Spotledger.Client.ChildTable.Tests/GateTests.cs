using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>
/// The draft layer enforces only the mask on a value write: it knows neither the caller's write
/// right nor that a posted document is closed. These tests pin that the table control is the gate:
/// every way a row can change is refused, with nothing changed and nothing journaled, when the
/// host has not granted the write right or the document state forbids the edit.
/// </summary>
public sealed class GateTests
{
    private static ChildTableResult Run(ChildTableModel model, string operation) => operation switch
    {
        "set-cell" => model.SetCell(0, Fixtures.Qty, new DecimalFieldValue(99m)),
        "undo" => model.Undo(),
        "redo" => model.Redo(),
        "add" => model.AddRow(),
        "insert" => model.InsertRow(1),
        "delete" => model.DeleteRows([0]),
        "move-up" => model.MoveRows([1], -1),
        "move-down" => model.MoveRows([0], 1),
        "duplicate" => model.DuplicateRows([0]),
        "paste" => model.Paste("5.00\t6.00", 0, Fixtures.Qty),
        "fill-down" => model.FillDown([0, 1], Fixtures.Qty),
        _ => throw new ArgumentOutOfRangeException(nameof(operation)),
    };

    /// <summary>Makes undo and redo meaningful: a journal with something to undo and redo.</summary>
    private static void SeedJournal(ChildTableModel model)
    {
        model.CanWrite = true;
        Assert.True(model.SetCell(2, Fixtures.Qty, new DecimalFieldValue(7m)).Succeeded);
        Assert.True(model.SetCell(2, Fixtures.Rate, new DecimalFieldValue(8m)).Succeeded);
        Assert.True(model.Undo().Succeeded);
    }

    private static void AssertRefusesEverything(ChildTableModel model, ChildTableRefusal expected)
    {
        var ids = model.Ids();
        var journal = model.Table.JournalCount;
        var canUndo = model.Table.CanUndo;
        var canRedo = model.Table.CanRedo;
        var dirty = model.Draft.DirtyPaths();
        var qty = model.Text(2, Fixtures.Qty);
        var events = 0;
        model.Draft.StructureChanged += (_, _) => events++;
        foreach (var operation in new[] { "set-cell", "undo", "redo", "add", "insert", "delete", "move-up", "move-down", "duplicate", "paste", "fill-down" })
        {
            var result = Run(model, operation);
            Assert.False(result.Succeeded, operation);
            Assert.Equal(expected, result.Refusal);
            Assert.False(string.IsNullOrEmpty(result.Message));
        }

        Assert.Equal(ids, model.Ids());
        Assert.Equal(journal, model.Table.JournalCount);
        Assert.Equal(canUndo, model.Table.CanUndo);
        Assert.Equal(canRedo, model.Table.CanRedo);
        Assert.Equal(dirty, model.Draft.DirtyPaths());
        Assert.Equal(0, events);
        Assert.Equal(qty, model.Text(2, Fixtures.Qty));
    }

    [Fact]
    public void HostWithoutWriteRightCannotChangeAnything()
    {
        var model = Fixtures.Model(Fixtures.Draft(), canWrite: false);
        // The write right is off by default: a fresh model is read-only.
        Assert.False(new ChildTableModel(Fixtures.Draft(), "items").IsEditable);
        SeedJournal(model);
        model.CanWrite = false;
        Assert.False(model.IsEditable);
        AssertRefusesEverything(model, ChildTableRefusal.NoWriteRight);
    }

    [Theory]
    [InlineData("posted", ChildTableRefusal.DocumentPosted)]
    [InlineData("cancelled", ChildTableRefusal.DocumentCancelled)]
    [InlineData("submitted-for-audit", ChildTableRefusal.DocumentStateUnknown)]
    public void ReadOnlyDocumentCannotBeChanged(string state, ChildTableRefusal expected)
    {
        // A draft that was edited, then the document arrives posted (or in a state we do not know).
        var draft = Fixtures.Draft(state: "draft");
        var model = Fixtures.Model(draft);
        SeedJournal(model);
        draft.LoadFrom($$"""{"_state":"{{state}}"}""");
        Assert.Equal(expected, model.TableRefusal);
        AssertRefusesEverything(model, expected);
    }

    [Theory]
    [InlineData("draft")]
    [InlineData(null)]
    public void DraftAndNewDocumentsAreEditable(string? state)
    {
        var model = Fixtures.Model(Fixtures.Draft(state: state));
        Assert.True(model.IsEditable);
        Assert.True(model.AddRow().Succeeded);
    }

    [Fact]
    public void ReadOnlyTableFieldCannotBeChanged()
    {
        var permissions = Fixtures.Permissions(items: """{"read_only":["items"]}""");
        var model = Fixtures.Model(Fixtures.Draft(permissions: permissions));
        Assert.Equal(ChildTableRefusal.TableReadOnly, model.TableRefusal);
        AssertRefusesEverything(model, ChildTableRefusal.TableReadOnly);
    }

    [Fact]
    public void MaskedCellRefusesEditAndPasteAndFill()
    {
        var permissions = Fixtures.Permissions(line: """{"masked":["rate"]}""");
        var draft = Fixtures.Draft(permissions: permissions);
        var model = Fixtures.Model(draft);
        Assert.Equal(ChildTableRefusal.CellMasked, model.CellRefusal(0, Fixtures.Rate));
        Assert.Equal(ChildTableRefusal.None, model.CellRefusal(0, Fixtures.Qty));

        var before = draft.DirtyPaths();
        Assert.Equal(ChildTableRefusal.CellMasked, model.SetCell(0, Fixtures.Rate, new DecimalFieldValue(1m)).Refusal);
        var paste = model.Paste("1.00\t2.00", 0, Fixtures.Qty);
        Assert.False(paste.Succeeded);
        var error = Assert.Single(paste.Paste!.Errors);
        Assert.Equal("rate", error.Column);
        Assert.Equal(1, error.Row);
        Assert.Equal(ChildTableRefusal.CellMasked, model.FillDown([0, 1], Fixtures.Rate).Refusal);
        Assert.Equal(before, draft.DirtyPaths());
        Assert.Equal("3.00", model.Text(2, Fixtures.Qty));
    }

    [Fact]
    public void ReadOnlyCellRefusesEditPasteAndFill()
    {
        var model = Fixtures.Model(Fixtures.Draft());
        // "amount" is read-only in the field model.
        Assert.Equal(ChildTableRefusal.CellReadOnly, model.CellRefusal(0, Fixtures.Amount));
        Assert.Equal(ChildTableRefusal.CellReadOnly, model.SetCell(0, Fixtures.Amount, new DecimalFieldValue(1m)).Refusal);
        Assert.Equal(ChildTableRefusal.CellReadOnly, model.FillDown([0, 1], Fixtures.Amount).Refusal);
        var paste = model.Paste("1.00\t2.00\t3.00", 0, Fixtures.Qty);
        Assert.False(paste.Succeeded);
        Assert.Equal("amount", Assert.Single(paste.Paste!.Errors).Column);
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void PermissionReadOnlyCellRefusesEdit()
    {
        var permissions = Fixtures.Permissions(line: """{"read_only":["qty"]}""");
        var model = Fixtures.Model(Fixtures.Draft(permissions: permissions));
        Assert.Equal(ChildTableRefusal.CellReadOnly, model.CellRefusal(1, Fixtures.Qty));
        Assert.Equal(ChildTableRefusal.CellReadOnly, model.SetCell(1, Fixtures.Qty, new DecimalFieldValue(1m)).Refusal);
        Assert.False(model.Draft.IsDirty);
    }

    [Fact]
    public void KindsThatNeedMoreThanACellAreNotEditedInline()
    {
        var area = new FieldModel("note", "Note", FieldKind.LongText);
        var many = new FieldModel("tags", "Tags", FieldKind.Select, options: ["a"], multiple: true);
        Assert.False(ChildTableModel.IsInlineEditable(area));
        Assert.False(ChildTableModel.IsInlineEditable(many));
        Assert.True(ChildTableModel.IsInlineEditable(new FieldModel("q", "Q", FieldKind.DecimalNumber)));
    }

    [Fact]
    public void RevokingTheRightRaisesEditabilityChanged()
    {
        var model = Fixtures.Model(Fixtures.Draft());
        var raised = 0;
        model.EditabilityChanged += (_, _) => raised++;
        model.CanWrite = false;
        model.CanWrite = false;
        model.CanWrite = true;
        Assert.Equal(2, raised);
    }

    [Fact]
    public void LoadingAPostedDocumentRaisesEditabilityChanged()
    {
        var draft = Fixtures.Draft();
        var model = Fixtures.Model(draft);
        var raised = 0;
        model.EditabilityChanged += (_, _) => raised++;
        draft.LoadFrom("""{"_state":"posted"}""");
        Assert.True(raised > 0);
        Assert.False(model.IsEditable);
    }

    [Fact]
    public void RefusedOperationsLeaveNoJournalEntry()
    {
        var model = Fixtures.Model(Fixtures.Draft(), canWrite: false);
        var before = model.Table.JournalCount;
        foreach (var operation in new[] { "set-cell", "add", "insert", "delete", "duplicate", "paste" })
        {
            Assert.False(Run(model, operation).Succeeded);
        }

        Assert.Equal(before, model.Table.JournalCount);
        Assert.False(model.Draft.IsDirty);
    }
}
