using System.Text.Json;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 8's draft behaviour: dirty tracking and revert, the child row lifecycle
/// (add, remove, reorder, kept ids, new-row marking), the merge-patch shapes (only
/// changed fields; tables replaced in full; per-row <c>idx</c>; unknown properties
/// preserved by not being touched), validation with the contract's row paths (rows from
/// 1), and the empty patch of an unchanged draft.
/// </summary>
public sealed class DocumentDraftTests
{
    [Fact]
    public void UnchangedPatchIsEmpty()
    {
        // A loaded draft that nobody edited produces {}
        var loaded = TestModels.LoadedInvoice();
        Assert.False(loaded.IsDirty);
        Assert.Empty(loaded.DirtyPaths());
        Assert.Equal("{}", loaded.ToPatch());

        // ... and so does a fresh draft nobody touched.
        Assert.Equal("{}", new DocumentDraft(TestModels.Invoice()).ToPatch());
    }

    [Fact]
    public void LoadedDraftIsCleanIncludingKindsWithZeroDefaults()
    {
        // Regression shape: the datetimes/select/json fields start from a "zero" default
        // before the load; the load makes the loaded value the original for every kind.
        var draft = TestModels.LoadedInvoice();
        Assert.False(draft.IsDirty);
        Assert.Equal("2026-09-12", Assert.IsType<DateFieldValue>(draft.Get("transaction_date")).Value.ToString("yyyy-MM-dd", System.Globalization.CultureInfo.InvariantCulture));
        Assert.Equal(new DateTimeOffset(2026, 9, 12, 10, 30, 0, TimeSpan.Zero), Assert.IsType<DateTimeFieldValue>(draft.Get("created_at")).Value);
        Assert.Equal(9007199254740993L, Assert.IsType<IntFieldValue>(draft.Get("opening_number")).Value);
        Assert.Equal("1250.00", Assert.IsType<DecimalFieldValue>(draft.Get("rate")).CanonicalText);
        Assert.Equal(new AttachFieldValue("ATT-0009"), draft.Get("attachments"));
        Assert.Equal("""{"keep":true}""", Assert.IsType<JsonFieldValue>(draft.Get("settings_json")).JsonText);
        // The unknown-kind field keeps its raw encoding as data: shown, never edited.
        Assert.Equal("inked", Assert.IsType<TextFieldValue>(draft.Get("signature")).Value);
        Assert.Equal("42", draft.UnknownProperties["note_field"]);
    }

    [Fact]
    public void SetMarksDirtyAndPatchCarriesOnlyTheChange()
    {
        var draft = TestModels.LoadedInvoice();
        draft.Set("customer", new LinkFieldValue("CUST-9999", "Beta LLC"));
        Assert.True(draft.IsDirty);
        Assert.Equal(["customer"], draft.DirtyPaths());
        Assert.Equal("""{"customer":"CUST-9999"}""", draft.ToPatch());
    }

    [Fact]
    public void SameValueSetIsNotDirty()
    {
        var draft = TestModels.LoadedInvoice();
        draft.Set("rate", new DecimalFieldValue(1250m));
        Assert.False(draft.IsDirty);
        Assert.Equal("{}", draft.ToPatch());
    }

    [Fact]
    public void RevertRestoresLoadedValues()
    {
        var draft = TestModels.LoadedInvoice();
        draft.Set("customer", new LinkFieldValue("CUST-9999", null));
        draft.Table("taxes").RemoveRowAt(1);
        Assert.True(draft.IsDirty);

        draft.Revert();
        Assert.False(draft.IsDirty);
        Assert.Equal("CUST-0001", Assert.IsType<LinkFieldValue>(draft.Get("customer")).Id);
        Assert.Equal(3, draft.Table("taxes").Rows.Count);
        Assert.Equal("row-0002", draft.Table("taxes").Rows[1].Id);
        Assert.Equal("{}", draft.ToPatch());
    }

    [Fact]
    public void MarkSavedRebasesTheOriginals()
    {
        var draft = TestModels.LoadedInvoice();
        draft.Set("status", new SelectFieldValue("Draft"));
        Assert.Equal("Paid", Assert.IsType<SelectFieldValue>(draft.Fields["status"].Original).Value);
        draft.MarkSaved();
        Assert.False(draft.IsDirty);
        Assert.Equal("Draft", Assert.IsType<SelectFieldValue>(draft.Fields["status"].Original).Value);
    }

    [Fact]
    public void EventsFireWithFieldPaths()
    {
        var draft = TestModels.LoadedInvoice();
        var changed = new List<string>();
        var structures = 0;
        draft.FieldChanged += (_, path) => changed.Add(path);
        draft.StructureChanged += (_, _) => structures++;

        draft.Set("status", new SelectFieldValue("Draft"));
        draft.LoadFrom(TestModels.DocumentJson);

        Assert.Equal(["status"], changed);
        Assert.Equal(1, structures);
    }

    [Fact]
    public void AddRemoveReorderRowsWithKeptIds()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        Assert.False(table.IsDirty);

        var added = table.AddRow();
        Assert.True(added.IsNew);
        Assert.Null(added.Id);
        added.SetValue("qty", new DecimalFieldValue(5m));
        Assert.True(table.IsDirty);

        // Reorder: move the new row before the first.
        table.MoveRow(3, 0);
        Assert.Equal([null, "row-0001", "row-0002", "row-0003"], table.Rows.Select(row => row.Id));

        // Remove the last kept row: gone from the rows (the patch's full replacement
        // deletes it server-side).
        table.RemoveRowAt(3); // row-0003
        Assert.Equal([(string?)null, "row-0001", "row-0002"], table.Rows.Select(row => row.Id));

        // Revert brings everything back, ids and order included.
        draft.Revert();
        Assert.Equal(["row-0001", "row-0002", "row-0003"], table.Rows.Select(row => row.Id));
        Assert.False(table.IsDirty);
    }

    [Fact]
    public void PatchShapeReplacesTableInFullWithIdxAndIds()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");

        // Change row 2's qty (kept id), remove row 1, add a new row at the end.
        table.Rows[1].SetValue("qty", new DecimalFieldValue(21.5m));
        table.RemoveRowAt(0);
        var added = table.AddRow();
        added.SetValue("qty", new DecimalFieldValue(99m));

        var patch = JsonDocument.Parse(draft.ToPatch());
        var rows = patch.RootElement.GetProperty("taxes");
        Assert.Equal(JsonValueKind.Array, rows.ValueKind);
        Assert.Equal(3, rows.GetArrayLength());

        using var enumerator = rows.EnumerateArray().GetEnumerator();
        enumerator.MoveNext();
        var first = enumerator.Current; // old row-0002 in position 1
        Assert.Equal("row-0002", first.GetProperty("id").GetString());
        Assert.Equal(1, first.GetProperty("idx").GetInt32());
        // The changed value, in the value's canonical form (no scale padding: the scale
        // lives in the field's metadata, not the wire text).
        Assert.Equal("21.5", first.GetProperty("qty").GetString());
        Assert.False(first.TryGetProperty("item_code", out _)); // unchanged row field stays out

        enumerator.MoveNext();
        var second = enumerator.Current; // kept row-0003 in position 2
        Assert.Equal("row-0003", second.GetProperty("id").GetString());
        Assert.Equal(2, second.GetProperty("idx").GetInt32());
        Assert.False(second.TryGetProperty("qty", out _)); // untouched

        enumerator.MoveNext();
        var third = enumerator.Current; // the new row: no id
        Assert.False(third.TryGetProperty("id", out _));
        Assert.Equal(3, third.GetProperty("idx").GetInt32());
        Assert.Equal("99", third.GetProperty("qty").GetString());
    }

    [Fact]
    public void AssignRowIdMarksTheNewRowServerNamed()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        var added = table.AddRow();
        Assert.True(table.AssignRowId(added, "row-0004"));
        Assert.False(added.IsNew);
        Assert.Equal("row-0004", added.Id);
        Assert.False(table.AssignRowId(new DocumentDraft.TableDraft.RowDraft(null), "nope"));
    }

    [Fact]
    public void AddedRowAtTheRequestedPositionLandsThere()
    {
        var loaded = TestModels.LoadedInvoice();
        loaded.Table("taxes").AddRowAt(1);
        Assert.Equal(["row-0001", null, "row-0002", "row-0003"], loaded.Table("taxes").Rows.Select(row => row.Id));
    }

    [Fact]
    public void DirtyRowPathsCountFromOne()
    {
        var draft = TestModels.LoadedInvoice();
        draft.Table("taxes").Rows[2].SetValue("qty", new DecimalFieldValue(31m));
        Assert.Equal(["taxes[3].qty"], draft.DirtyPaths());
    }

    [Fact]
    public void ValidationReportsRequiredMaxLengthOptionsAndScaleWithRowPaths()
    {
        var draft = TestModels.LoadedInvoice();
        draft.Set("customer", FieldValue.Null); // required
        draft.Set("billing_email", new TextFieldValue("not-an-email")); // format email
        draft.Set("message", new TextFieldValue(new string('x', 141))); // max chars
        draft.Set("status", new SelectFieldValue("Shipped")); // not an option
        draft.Table("taxes").AddRow().SetValue("qty", new DecimalFieldValue(1.234m)); // row 4: scale 2
        draft.Table("taxes").Rows[0].SetValue("item_code", FieldValue.Null); // required row field, row 1

        var errors = DraftValidator.Validate(draft);

        Assert.Equal(
            ["customer", "taxes[1].item_code", "taxes[4].item_code"],
            errors.Where(e => e.Code == "document.required").Select(e => e.Field).ToList());
        Assert.Equal("billing_email", errors.Single(e => e.Code == "document.invalid_format").Field);
        Assert.Equal("message", errors.Single(e => e.Code == "document.max_length").Field);
        Assert.Equal("status", errors.Single(e => e.Code == "document.not_an_option").Field);
        Assert.Equal("taxes[4].qty", errors.Single(e => e.Code == "document.scale_exceeded").Field);
        Assert.Contains("is limited to 2 decimal places", errors.Single(e => e.Code == "document.scale_exceeded").Message);
    }

    [Fact]
    public void ValidatorNeverRoundsAnOverScaleDocumentValue()
    {
        // The server (or a document) can carry more places than the field's scale: the
        // validator reports the error instead of rounding it away.
        var draft = TestModels.LoadedInvoice();
        draft.Table("taxes").Rows[0].SetValue("qty", new DecimalFieldValue(
            ExactDecimal.FromCanonical("10.001")));
        var errors = DraftValidator.Validate(draft);
        var scaleError = Assert.Single(errors, error => error.Code == "document.scale_exceeded");
        Assert.Equal("taxes[1].qty", scaleError.Field);
        Assert.Equal("10.001", Assert.IsType<DecimalFieldValue>(draft.Table("taxes").Rows[0].GetValue("qty")).CanonicalText);
    }
}