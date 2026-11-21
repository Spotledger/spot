using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 1 (WP-UI-07.T4): the draft reports every child-row mutation. One
/// notification per call with the payload a table control needs — the kind, the table,
/// the row keys and the position range — and one notification for a bulk operation,
/// never one per cell. The document-wide changes (load, revert, save-rebase, permission
/// refresh) keep their single batched notification.
/// </summary>
public sealed class RowStructureEventTests
{
    [Fact]
    public void CellEditRaisesOneFieldAndOneStructureNotification()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var fields = new List<string>();
        var structures = new List<StructureChangedEventArgs>();
        draft.FieldChanged += (_, path) => fields.Add(path);
        draft.StructureChanged += (_, change) => structures.Add(change);

        table.Rows[1].SetValue("qty", new DecimalFieldValue(21.5m));

        // The legacy field path (rows from 1) and the payload with the row key.
        Assert.Equal(["taxes[2].qty"], fields);
        var change = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.CellEdited, change.Kind);
        Assert.Equal("taxes", change.Table);
        Assert.Equal("qty", change.Field);
        Assert.Equal(["row-0002"], change.RowKeys);
        Assert.Equal(1, change.Index);
        Assert.Equal(1, change.Count);
        Assert.False(change.IsUndo);
        Assert.False(change.IsRedo);
    }

    [Fact]
    public void AddRowRaisesOneStructureNotificationWithTheNewRowsPosition()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        var fields = new List<string>();
        draft.StructureChanged += (_, change) => structures.Add(change);
        draft.FieldChanged += (_, path) => fields.Add(path);

        var added = table.AddRow();

        var change = Assert.Single(structures);
        Assert.Empty(fields);
        Assert.Equal(StructureChangeKind.RowAdded, change.Kind);
        Assert.Equal("taxes", change.Table);
        Assert.Equal([added.Id], change.RowKeys);
        Assert.Null(added.Id);
        Assert.Equal(3, change.Index);
        Assert.Equal(3, change.LastIndex);
    }

    [Fact]
    public void AddRowAtRaisesOneInsertNotificationWithTheInsertedPosition()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        table.AddRowAt(1);

        var change = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.RowInserted, change.Kind);
        Assert.Equal(1, change.Index);
        Assert.Equal(["row-0001", null, "row-0002", "row-0003"], table.Rows.Select(row => row.Id));
    }

    [Fact]
    public void RemoveRowRaisesOneNotificationWithTheRemovedRowsIdAndIndex()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        table.RemoveRowAt(0);

        var change = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.RowRemoved, change.Kind);
        Assert.Equal(["row-0001"], change.RowKeys);
        Assert.Equal(0, change.Index);
    }

    [Fact]
    public void MoveRowRaisesOneNotificationWithBothPositions()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        table.MoveRow(0, 2);

        var change = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.RowMoved, change.Kind);
        Assert.Equal(["row-0001"], change.RowKeys);
        Assert.Equal(0, change.FromIndex);
        Assert.Equal(2, change.Index);
        // The id travels with the row: the notification is re-keyed, not re-numbered.
        Assert.Equal(["row-0002", "row-0003", "row-0001"], table.Rows.Select(row => row.Id));
    }

    [Fact]
    public void LoadRowsRaisesOneNotificationListingTheLoadedKeys()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        table.LoadRows(new TableFieldValue(
            [
                new TableRowValue("new-1", new Dictionary<string, FieldValue>(StringComparer.Ordinal)
                {
                    ["qty"] = new DecimalFieldValue(4m),
                }),
            ],
            TestModels.TaxesRowModel()));

        var change = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.RowsLoaded, change.Kind);
        Assert.Equal(["new-1"], change.RowKeys);
        Assert.Equal(1, change.Count);
    }

    [Fact]
    public void DocumentLoadRaisesOneBatchedNotification()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        draft.LoadFrom(TestModels.DocumentJson);

        var change = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.DocumentLoaded, change.Kind);
        Assert.Null(change.Table);
    }

    [Fact]
    public void RevertAndMarkSavedRaiseOneBatchedNotificationEach()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        table.Rows[0].SetValue("qty", new DecimalFieldValue(1m));
        table.AddRow();
        draft.StructureChanged += (_, change) => structures.Add(change);

        draft.Revert();
        var revert = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.Reverted, revert.Kind);

        table.Rows[0].SetValue("qty", new DecimalFieldValue(2m));
        structures.Clear();
        draft.MarkSaved();
        var saved = Assert.Single(structures);
        Assert.Equal(StructureChangeKind.Saved, saved.Kind);
    }

    [Fact]
    public void BulkOperationRaisesOneNotificationForTheWholeCall()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        var fields = new List<string>();
        draft.StructureChanged += (_, change) => structures.Add(change);
        draft.FieldChanged += (_, path) => fields.Add(path);

        using (var operation = table.Begin("paste 2 rows"))
        {
            var first = table.AddRow();
            first.SetValue("item_code", new LinkFieldValue("ITEM-9", null));
            first.SetValue("qty", new DecimalFieldValue(5m));
            var second = table.AddRow();
            second.SetValue("qty", new DecimalFieldValue(6m));

            // Inside the operation nothing is reported: a paste is one change to the user.
            Assert.Empty(structures);
            Assert.Empty(fields);
            operation.Commit();
        }

        var change = Assert.Single(structures);
        Assert.Empty(fields);
        Assert.Equal(StructureChangeKind.BulkOperation, change.Kind);
        Assert.Equal("paste 2 rows", change.Description);
        Assert.Equal(5, change.Count); // two inserts and three cell edits
        Assert.Equal(3, change.Index);
        Assert.Equal(4, change.LastIndex);
    }

    [Fact]
    public void AnOperationCommittedTwiceReportsOnce()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        var operation = table.Begin("fill down");
        table.Rows[1].SetValue("qty", new DecimalFieldValue(20m));
        operation.Commit();
        operation.Commit();
        operation.Dispose();

        Assert.Single(structures);
        Assert.Equal(1, table.JournalCount);
    }

    [Fact]
    public void NestedOperationsOnOneTableAreRefused()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        using var outer = table.Begin("outer");

        Assert.Throws<InvalidOperationException>(() => table.Begin("inner"));
    }

    [Fact]
    public void UndoAndRedoRaiseTheOperationKindsAgainWithTheDirectionFlagged()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var structures = new List<StructureChangedEventArgs>();
        draft.StructureChanged += (_, change) => structures.Add(change);

        table.RemoveRowAt(1);
        Assert.True(table.UndoRow());
        Assert.True(table.RedoRow());

        Assert.Equal(3, structures.Count);
        Assert.Equal(StructureChangeKind.RowRemoved, structures[0].Kind);
        Assert.Equal(StructureChangeKind.RowRemoved, structures[1].Kind);
        Assert.True(structures[1].IsUndo);
        Assert.False(structures[1].IsRedo);
        Assert.Equal(StructureChangeKind.RowRemoved, structures[2].Kind);
        Assert.True(structures[2].IsRedo);
        Assert.Equal(["row-0002"], structures[1].RowKeys);
    }
}
