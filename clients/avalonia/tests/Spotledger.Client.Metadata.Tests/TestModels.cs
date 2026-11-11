using Spotledger.Client.Metadata;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// The DocType model every draft test edits, written directly with the runtime's public
/// constructors (the server-side shape WP-MET-01 fetches separately): one field of every
/// kind the contract defines, the child table "taxes", a max-limited text, a
/// formatted text, and a required link. The document JSON below loads into it.
/// </summary>
internal static class TestModels
{
    /// <summary>A document the <see cref="Invoice"/> model loads: the id, the version,
    /// one value of every kind, three child rows, a field the model predates, and an
    /// embedded JSON object.</summary>
    public const string DocumentJson =
        """{"id":"INV-0001","version":9,"transaction_date":"2026-09-12","rate":"1250.00","grand_total":"1250.00","billing_email":"bill@example.com","customer":"CUST-0001","status":"Paid","is_return":true,"message":"hello","terms":"plenty","opening_number":"9007199254740993","created_at":"2026-09-12T10:30:00Z","attachments":"ATT-0009","signature":"inked","bytes_field":"AAAA","settings_json":{"keep":true},"taxes":[{"id":"row-0001","idx":1,"item_code":"ITEM-1","qty":"10.00"},{"id":"row-0002","idx":2,"item_code":"ITEM-2","qty":"20.00"},{"id":"row-0003","idx":3,"item_code":"ITEM-3","qty":"30.00"}],"note_field":42}""";

    /// <summary>The DocType model of the tests' invoice.</summary>
    public static DocTypeModel Invoice() => new(
        "sales_invoice",
        "Sales Invoice",
        version: 7,
        isPosted: true,
        fields:
        [
            new FieldModel("transaction_date", "Date", FieldKind.Date),
            new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("grand_total", "Grand Total", FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true),
            new FieldModel("billing_email", "Billing E-mail", FieldKind.Text, format: "email"),
            new FieldModel("customer", "Customer", FieldKind.Link, linkTo: "customer", required: true),
            new FieldModel("status", "Status", FieldKind.Select, options: ["Draft", "Paid"]),
            new FieldModel("is_return", "Is Return", FieldKind.Bool),
            new FieldModel("message", "Message", FieldKind.Text, maxChars: 140),
            new FieldModel("terms", "Terms", FieldKind.LongText),
            new FieldModel("opening_number", "Opening Number", FieldKind.IntegerNumber),
            new FieldModel("created_at", "Created At", FieldKind.Datetime),
            new FieldModel("attachments", "Attachments", FieldKind.Attach),
            new FieldModel("signature", "Signature", FieldKind.Unknown, unknownKindName: "signature_pad"),
            new FieldModel("bytes_field", "Bytes", FieldKind.Bytes),
            new FieldModel("settings_json", "Settings", FieldKind.Json),
            new FieldModel("taxes", "Taxes", FieldKind.Table, childDoctype: "sales_invoice_item"),
        ],
        childTables: new Dictionary<string, DocTypeModel>(StringComparer.Ordinal)
        {
            ["taxes"] = TaxesRowModel(),
        });

    /// <summary>The row DocType's model (loaded separately in production; assembled here).</summary>
    public static DocTypeModel TaxesRowModel() => new(
        "sales_invoice_item",
        "Sales Invoice Item",
        version: 0,
        isPosted: false,
        fields:
        [
            new FieldModel("item_code", "Item", FieldKind.Link, linkTo: "item", required: true),
            new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2, precision: 12, required: true),
        ]);

    /// <summary>A draft loaded from <see cref="DocumentJson"/> (clean, values in place).</summary>
    public static DocumentDraft LoadedInvoice() => new DocumentDraft(Invoice(), version: 1).Also(
        draft => draft.LoadFrom(DocumentJson));

    private static T Also<T>(this T value, Action<T> action)
    {
        action(value);
        return value;
    }
}