using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

public sealed class PresentationLifecycleTests
{
    [Fact]
    public void MultipleDefaultsAndAllScalarDefaultsSurviveLosslessMetadataDecoder()
    {
        var model = MetadataResponseDecoder.Decode("""
            {"data":{"doctype":"record","label":"Record","version":1,"fields":[
              {"name":"tags","label":"Tags","type":"text","select":["a","b"],"multiple":true,"default":["a"]},
              {"name":"title","label":"Title","type":"text","default":"hello"},
              {"name":"number","label":"Number","type":"int","default":42},
              {"name":"enabled","label":"Enabled","type":"bool","default":true},
              {"name":"empty","label":"Empty","type":"text","default":null}
            ]}}
            """);
        var draft = new DocumentDraft(model);
        Assert.Equal(["a"], Assert.IsType<MultiSelectFieldValue>(draft.Get("tags")).Values);
        Assert.Equal(new TextFieldValue("hello"), draft.Get("title"));
        Assert.Equal(new IntFieldValue(42), draft.Get("number"));
        Assert.Equal(new BoolFieldValue(true), draft.Get("enabled"));
        Assert.False(draft.IsDirty);
        Assert.Equal("{}", draft.ToPatch());
        draft.TrySetText("tags", """["b"]""");
        var encoded = draft.ToPatch();
        draft.LoadSavedFrom(encoded);
        Assert.Equal(["b"], Assert.IsType<MultiSelectFieldValue>(draft.Get("tags")).Values);
        Assert.False(draft.IsDirty);
    }

    [Fact]
    public void RevokedChildValuesCannotReappearThroughRevertOrTheAggregate()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        table.Rows[0].SetValue("qty", new DecimalFieldValue(99m));
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"masked","data":{"doctypes":[{"doctype":"sales_invoice_item","rights":{},
            "fields":{"masked":["qty"],"revealable":["qty"]}}]}}
            """));
        Assert.All(table.Rows, row => Assert.IsType<NullFieldValue>(row.GetValue("qty")));
        Assert.All(Assert.IsType<TableFieldValue>(draft.Get("taxes")).Rows, row => Assert.IsType<NullFieldValue>(row.Values["qty"]));
        draft.Revert();
        Assert.All(table.Rows, row =>
        {
            Assert.True(row.Values["qty"].IsMasked);
            Assert.False(row.Values["qty"].IsEditable);
            Assert.IsType<NullFieldValue>(row.Values["qty"].Original);
            Assert.IsType<NullFieldValue>(row.Values["qty"].Value);
        });
        Assert.DoesNotContain(DraftValidator.Validate(draft), error => error.Field.EndsWith(".qty", StringComparison.Ordinal));
        Assert.Equal("{}", draft.ToPatch());
    }

    [Fact]
    public void AttachmentConstraintsResolveExactChildPathsWithoutClientScopeMerging()
    {
        var child = new DocTypeModel("row", "Row", 1, false,
            [new FieldModel("file", "File", FieldKind.Attach)]);
        var model = new DocTypeModel("record", "Record", 1, false,
            [new FieldModel("rows", "Rows", FieldKind.Table, childDoctype: "row")],
            new Dictionary<string, DocTypeModel> { ["rows"] = child });
        var draft = new DocumentDraft(model);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"limits1","data":{"doctypes":[
              {"doctype":"record","rights":{},"fields":{},"attach_limits":{"rows[2].file":{"max_bytes":9000,"accept":["image/png"]}}},
              {"doctype":"row","rights":{},"fields":{},"attach_limits":{"file":{"max_bytes":1000}}}]}}
            """));
        draft.LoadFrom("""{"rows":[{"id":"one","file":"a"},{"id":"two","file":"b"}]}""");
        var rows = draft.Table("rows").Rows;
        Assert.Equal(1000, rows[0].Values["file"].AttachLimit!.MaxBytes);
        Assert.Equal(9000, rows[1].Values["file"].AttachLimit!.MaxBytes);
        Assert.Equal(["image/png"], rows[1].Values["file"].AttachLimit!.Accept);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"limits2","data":{"doctypes":[]}}"""));
        Assert.All(rows, row => Assert.Null(row.Values["file"].AttachLimit));
    }

    [Fact]
    public void MaskingAnEntireTableScrubsItsRowsAndPreventsAnyWrite()
    {
        var draft = TestModels.LoadedInvoice();
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"hidden-table","data":{"doctypes":[{"doctype":"sales_invoice","rights":{},"fields":{"masked":["taxes"]}}]}}
            """));
        Assert.Empty(draft.Table("taxes").Rows);
        Assert.IsType<NullFieldValue>(draft.Get("taxes"));
        draft.LoadFrom(TestModels.DocumentJson);
        Assert.Empty(draft.Table("taxes").Rows);
        draft.Table("taxes").AddRow().SetValue("qty", new DecimalFieldValue(6m));
        using var patch = JsonDocument.Parse(draft.ToPatch());
        Assert.False(patch.RootElement.TryGetProperty("taxes", out _));
        draft.Revert();
        Assert.Empty(draft.Table("taxes").Rows);
    }

    [Fact]
    public void ReadOrSaveClearsTransientRevealsEvenWhenFieldIsAbsent()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"reveals","data":{"doctypes":[{"doctype":"sales_invoice","rights":{},"fields":{"masked":["rate"],"revealable":["rate"]}}]}}
            """));
        draft.LoadFrom("""{"rate":"***"}""");
        draft.ApplyRevealResponse("rate", """{"data":{"value":null}}""");
        Assert.IsType<NullFieldValue>(draft.Fields["rate"].RevealedValue);
        draft.LoadSavedFrom("""{"version":2}""");
        Assert.Null(draft.Fields["rate"].RevealedValue);
        Assert.Null(draft.Fields["rate"].MaskedText);
        draft.LoadFrom("""{"rate":"***"}""");
        draft.ApplyRevealResponse("rate", """{"data":{"value":"10"}}""");
        draft.MarkSaved();
        Assert.Null(draft.Fields["rate"].RevealedValue);
    }

    [Fact]
    public void OpaqueMissingEtagsNeverSuppressARefreshAndNewLimitsMayDisappear()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"data":{"doctypes":[{"doctype":"customer","rights":{"create":true},"fields":{}}]}}
            """));
        Assert.True(draft.Fields["customer"].CanCreateTarget);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"data":{"doctypes":[]}}"""));
        Assert.False(draft.Fields["customer"].CanCreateTarget);
    }
}
