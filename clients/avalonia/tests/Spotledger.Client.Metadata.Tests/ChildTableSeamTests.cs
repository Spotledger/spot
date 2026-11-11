using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// The four characterizations WP-UI-08.T3's prerequisite audit wrote against this draft
/// (<c>PrerequisiteAuditTests.cs</c> on <c>task/WP-UI-08.T3</c>), ported here and flipped:
/// each one asserted the seam was <b>missing</b>, and each now asserts it exists. They
/// stay close to the audit's own fixture — a <c>record</c> with one <c>line</c> child
/// table — so the change of behaviour is read against the same setup.
/// </summary>
public sealed class ChildTableSeamTests
{
    [Fact]
    public void RowEditNotifiesDocumentSubscribers()
    {
        var draft = CreateDraft();
        var notifications = new List<string>();
        var structures = new List<StructureChangedEventArgs>();
        draft.FieldChanged += (_, path) => notifications.Add(path);
        draft.StructureChanged += (_, change) => structures.Add(change);

        draft.Table("items").Rows[0].SetValue("qty", new DecimalFieldValue(2m));

        Assert.True(draft.IsDirty);
        Assert.Contains("items[1].qty", draft.DirtyPaths());
        // Was: "Assert.Empty(notifications)" — the seam is the point of this task.
        Assert.Equal(["items[1].qty"], notifications);
        var structure = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.CellEdited, structure.Kind);
        Assert.Equal("items", structure.Table);
        Assert.Equal("01900000-0000-7000-8000-000000000001", Assert.Single(structure.RowKeys));
    }

    [Fact]
    public void RowOperationsNotifyAndUndoAsOneEach()
    {
        var draft = CreateDraft();
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);
        var table = draft.Table("items");
        var originalId = table.Rows[0].Id;

        table.AddRow();
        Assert.Equal(StructureChangeKind.RowAdded, Assert.Single(structures).Kind);
        structures.Clear();

        table.MoveRow(0, 1);
        Assert.Equal(originalId, table.Rows[1].Id); // ids are retained on reorder (ADR-057)
        Assert.Equal(StructureChangeKind.RowMoved, Assert.Single(structures).Kind);
        structures.Clear();

        table.RemoveRowAt(1); // the loaded row, now second
        var removed = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.RowRemoved, removed.Kind);
        Assert.Equal([originalId], removed.RowKeys);
        structures.Clear();

        table.AddRowAt(0);
        Assert.Equal(StructureChangeKind.RowInserted, Assert.Single(structures).Kind);

        Assert.True(draft.IsDirty);
        // Was: "Assert.Equal(0, notifications)" and an un-undoable table.
        Assert.True(table.CanUndo);

        // Undo every step back to the loaded row.
        Assert.True(table.UndoRow()); // insert
        Assert.True(table.UndoRow()); // delete: the removed row returns at index 0
        Assert.Equal([null, originalId], table.Rows.Select(row => row.Id));
        Assert.True(table.UndoRow()); // move
        Assert.True(table.UndoRow()); // add
        Assert.Equal([originalId], table.Rows.Select(row => row.Id));
        Assert.False(draft.IsDirty);
    }

    [Fact]
    public void RowLevelServerErrorLandsOnTheRowAndStaysInTheDocumentMessages()
    {
        var draft = CreateDraft();
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        var result = ErrorMapper.Apply(draft, new ApiError
        {
            Code = "document.validation_failed",
            Message = "Validation failed",
            FieldErrors = [new ApiFieldError("items[1]", "document.validation_failed", "Check row")],
        });

        // Was: the row error was dropped on the way to the row.
        var row = draft.Table("items").Rows[0];
        Assert.Equal("Check row", Assert.Single(row.RowErrors).Message);
        Assert.Equal(0, result.AppliedFieldCount);
        Assert.Equal(1, result.AppliedRowErrorCount);
        // Still reported at document level, so the existing callers keep working.
        Assert.Equal("items[1]", Assert.Single(result.DocumentMessages).Field);
        Assert.Single(events);
        Assert.Empty(row.Values["qty"].Errors);
    }

    [Fact]
    public void CellLevelServerErrorIsStoredOnTheCorrectCellAndAnnounced()
    {
        var draft = CreateDraft();
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        var result = ErrorMapper.Apply(draft, new ApiError
        {
            Code = "document.validation_failed",
            Message = "Validation failed",
            FieldErrors = [new ApiFieldError("items[1].qty", "document.validation_failed", "Check quantity")],
        });

        Assert.Equal(1, result.AppliedFieldCount);
        Assert.Equal(0, result.AppliedRowErrorCount);
        Assert.Empty(result.DocumentMessages);
        Assert.Equal("Check quantity", Assert.Single(draft.Table("items").Rows[0].Values["qty"].Errors).Message);
        var change = Assert.Single(events);
        Assert.Equal("qty", change.Field);
        Assert.Equal("01900000-0000-7000-8000-000000000001", change.RowKey);
    }

    /// <summary>The audit's fixture, unchanged: one parent, one child table with one row.</summary>
    private static DocumentDraft CreateDraft()
    {
        var row = new DocTypeModel("line", "Line", 1, false,
            [new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2)]);
        var parent = new DocTypeModel("record", "Record", 1, false,
            [new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line")],
            new Dictionary<string, DocTypeModel> { ["items"] = row });
        var draft = new DocumentDraft(parent);
        draft.LoadFrom("""{"items":[{"id":"01900000-0000-7000-8000-000000000001","qty":"1.00"}]}""");
        return draft;
    }
}
