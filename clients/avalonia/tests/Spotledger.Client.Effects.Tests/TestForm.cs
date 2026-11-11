using System.Text.Json;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects.Tests;

/// <summary>
/// A small DocType for the behaviour tests: the same shape as the corpus's <c>order</c> (an
/// int, three decimals, text with a limit, long text, bool, date, a link, a select and one
/// child table), so a test reads like the corpus case it extends.
/// </summary>
internal static class TestForm
{
    private static readonly string[] StatusOptions = { "draft", "open", "closed" };

    /// <summary>The DocType's model.</summary>
    public static DocTypeModel Model()
    {
        var fields = new List<FieldModel>
        {
            new(name: "qty", label: "qty", kind: FieldKind.IntegerNumber),
            new(name: "rate", label: "rate", kind: FieldKind.DecimalNumber, scale: 2, precision: 18),
            new(name: "amount", label: "amount", kind: FieldKind.DecimalNumber, scale: 2, precision: 18),
            new(name: "title", label: "title", kind: FieldKind.Text, maxChars: 20),
            new(name: "note", label: "note", kind: FieldKind.LongText),
            new(name: "active", label: "active", kind: FieldKind.Bool),
            new(name: "due", label: "due", kind: FieldKind.Date),
            new(name: "customer", label: "customer", kind: FieldKind.Link, linkTo: "customer"),
            new(name: "status", label: "status", kind: FieldKind.Select, options: StatusOptions),
            new(name: "items", label: "items", kind: FieldKind.Table, childDoctype: "item"),
        };

        var row = new DocTypeModel(
            "item",
            "item",
            0,
            false,
            new List<FieldModel>
            {
                new(name: "qty", label: "qty", kind: FieldKind.IntegerNumber),
                new(name: "unit_price", label: "unit_price", kind: FieldKind.DecimalNumber, scale: 2, precision: 18),
                new(name: "line_total", label: "line_total", kind: FieldKind.DecimalNumber, scale: 2, precision: 18),
                new(name: "note", label: "note", kind: FieldKind.Text, maxChars: 10),
            });

        return new DocTypeModel(
            "order",
            "Order",
            1,
            false,
            fields,
            new Dictionary<string, DocTypeModel>(StringComparer.Ordinal) { ["items"] = row });
    }

    /// <summary>A loaded form state: the corpus's order with a gross of 20.00.</summary>
    /// <param name="withRow">Whether the document has one <c>items</c> row.</param>
    public static FormState Load(bool withRow = false)
    {
        DocumentDraft draft = Draft(withRow);
        return new FormState(draft);
    }

    /// <summary>The draft <see cref="Load"/> wraps (for the tests that inspect dirty state).</summary>
    public static DocumentDraft Draft(bool withRow = false)
    {
        var draft = new DocumentDraft(Model(), "01900000-0000-7000-8000-0000000000aa");
        string json = withRow
            ? """
              {
                "id": "01900000-0000-7000-8000-0000000000aa",
                "qty": 2,
                "rate": "10.00",
                "amount": "20.00",
                "title": "first",
                "note": "hello",
                "active": true,
                "due": "2026-10-20",
                "customer": "01900000-0000-7000-8000-000000000001",
                "status": "draft",
                "items": [
                  { "id": "01900000-0000-7000-8000-0000000000b1", "qty": 1, "unit_price": "10.00", "line_total": "10.00", "note": "row one" }
                ]
              }
              """
            : """
              {
                "id": "01900000-0000-7000-8000-0000000000aa",
                "qty": 2,
                "rate": "10.00",
                "amount": "20.00",
                "title": "first",
                "note": "hello",
                "active": true,
                "due": "2026-10-20",
                "customer": "01900000-0000-7000-8000-000000000001",
                "status": "draft"
              }
              """;
        draft.LoadFrom(json);
        return draft;
    }

    /// <summary>Reads one effect out of JSON, failing the test when the wire refuses it.</summary>
    public static Effect Effect(string json)
    {
        using JsonDocument document = JsonDocument.Parse(json);
        return EffectWire.TryReadEffect(document.RootElement, out Effect? effect, out EffectProblem? problem) && effect is not null
            ? effect
            : throw new InvalidOperationException($"the test effect does not read: {problem}");
    }

    /// <summary>A batch of one effect written as JSON.</summary>
    public static EffectBatch Batch(params string[] effects) =>
        EffectBatch.Of(Array.AsReadOnly(effects.Select(Effect).ToArray()));

    /// <summary>The value at a path, as its wire text (for assertions).</summary>
    public static string? Text(FormState form, string path)
    {
        FieldValue? value = form.ValueAt(path);
        return value switch
        {
            null or NullFieldValue => null,
            IntFieldValue number => number.Value.ToString(System.Globalization.CultureInfo.InvariantCulture),
            DecimalFieldValue exact => exact.CanonicalText,
            TextFieldValue text => text.Value,
            SelectFieldValue select => select.Value,
            BoolFieldValue flag => flag.Value ? "true" : "false",
            LinkFieldValue link => link.Id,
            DateFieldValue date => date.Value.ToString("yyyy-MM-dd", System.Globalization.CultureInfo.InvariantCulture),
            _ => value.ToString(),
        };
    }
}
