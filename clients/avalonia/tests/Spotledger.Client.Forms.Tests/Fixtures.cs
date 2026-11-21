using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms.Tests;

public static class Fixtures
{
    /// <summary>An invoice: a required title, a link, an amount, a status select with a default, and
    /// a child table of lines (item link, quantity).</summary>
    public static DocTypeModel Model()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
        [
            new FieldModel("item", "Item", FieldKind.Text),
            new FieldModel("qty", "Qty", FieldKind.DecimalNumber, scale: 2, precision: 18, required: true),
        ]);
        return new DocTypeModel("invoice", "Invoice", 1, true,
        [
            new FieldModel("title", "Title", FieldKind.Text, required: true),
            new FieldModel("customer", "Customer", FieldKind.Link, linkTo: "party"),
            new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("priority", "Priority", FieldKind.Select, options: ["Low", "Normal", "High"], defaultValue: new SelectFieldValue("Normal")),
            new FieldModel("secret_note", "Private note", FieldKind.Text),
            new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line"),
        ],
        new Dictionary<string, DocTypeModel> { ["items"] = line });
    }

    public static PermissionSnapshot Permissions(string invoiceFields = "{}")
        => PermissionSnapshot.Decode(
            $$$"""{"etag":"e1","data":{"doctypes":[{"doctype":"invoice","rights":{"create":true,"write":true},"fields":{{{invoiceFields}}}},{"doctype":"party","rights":{"create":false},"fields":{}}]}}""");

    public static DocumentFormViewModel Form(FakeGateway gateway, string? id = null, FakeShell? shell = null)
        => new(gateway, "invoice", id, shell ?? new FakeShell());

    public static async Task<DocumentFormViewModel> OpenAsync(FakeGateway gateway, string? id, FakeShell? shell = null)
    {
        var form = Form(gateway, id, shell);
        await form.LoadAsync();
        return form;
    }
}
