using System.Text.Json;

namespace Spotledger.Client.Metadata;

/// <summary>
/// A typed value of one field, per its kind (the brief's "a FieldValue model per
/// kind"): null, bool, int64 (from the contract's string encoding), the exact decimal,
/// text, date, time, datetime, select (a text in the option list), link (id plus
/// display text when known), json (kept as the JSON text), bytes (base64 text), attach
/// reference, secret-set flag, and a child table. Never a floating point number: the
/// contract encodes every decimal as a string and this model keeps it exact.
/// </summary>
public abstract record FieldValue
{
    /// <summary>The single null value; distinct from an empty text (the contract's
    /// storage never converts one into the other).</summary>
    public static readonly FieldValue Null = new NullFieldValue();

    /// <summary>The zero-ish default for the value kinds (0, empty text, false).</summary>
    public static readonly FieldValue Zero = new IntFieldValue(0);

    /// <summary>True when the value is the null value.</summary>
    public bool IsNull => this is NullFieldValue;
}

/// <summary>The null value.</summary>
public sealed record NullFieldValue : FieldValue
{
    internal NullFieldValue() { }

    /// <inheritdoc/>
    public override string ToString() => string.Empty;
}

/// <summary>A boolean value.</summary>
/// <param name="Value">True or false.</param>
public sealed record BoolFieldValue(bool Value) : FieldValue;

/// <summary>A 64-bit integer value.</summary>
/// <param name="Value">The integer.</param>
public sealed record IntFieldValue(long Value) : FieldValue;

/// <summary>
/// An exact decimal value. Carries a <see cref="System.Decimal"/> when the value fits
/// the native 28 significant digits and the string-backed <see cref="ExactDecimal"/>
/// when it does not (the 29-digit case); the rule decides by the field, and the value
/// always compares exactly.
/// </summary>
public sealed record DecimalFieldValue : FieldValue
{
    /// <summary>Creates a value from the field's parsed exact decimal.</summary>
    /// <param name="Exact">The exact value (native or string-backed).</param>
    public DecimalFieldValue(ExactDecimal Exact)
    {
        ExactValue = Exact;
    }

    /// <summary>Creates a native-precision value.</summary>
    /// <param name="value">The decimal.</param>
    public DecimalFieldValue(decimal value)
    {
        ExactValue = ExactDecimal.FromDecimal(value);
    }

    /// <summary>The exact value.</summary>
    public ExactDecimal ExactValue { get; init; }

    /// <summary>True when the value is carried by a <see cref="System.Decimal"/>.</summary>
    public bool IsNative => ExactValue.IsNative;

    /// <summary>The canonical encoding text (the string a patch body carries).</summary>
    public string CanonicalText => ExactValue.ToCanonicalText();

    /// <inheritdoc/>
    public override string ToString() => CanonicalText;
}

/// <summary>A text value.</summary>
/// <param name="Value">The text.</param>
public sealed record TextFieldValue(string Value) : FieldValue;

/// <summary>A select value: a text that must be one of the field's options.</summary>
/// <param name="Value">The chosen option (the encoded value, not the label).</param>
public sealed record SelectFieldValue(string Value) : FieldValue;

/// <summary>The ordered option texts of a multiple select, never document references.</summary>
/// <param name="Values">Selected options in metadata order, without duplicates.</param>
public sealed record MultiSelectFieldValue(IReadOnlyList<string> Values) : FieldValue;

/// <summary>A date value.</summary>
/// <param name="Value">The date.</param>
public sealed record DateFieldValue(DateOnly Value) : FieldValue;

/// <summary>A time value.</summary>
/// <param name="Value">The time of day.</param>
public sealed record TimeFieldValue(TimeOnly Value) : FieldValue;

/// <summary>A datetime value.</summary>
/// <param name="Value">The instant with its offset.</param>
public sealed record DateTimeFieldValue(DateTimeOffset Value) : FieldValue;

/// <summary>
/// A link value: the target document's id and, when known (picked from a lookup or
/// fetched back), its display text. A dynamic link carries the same shape; which
/// DocType the id names is decided by the link's type field, not by the value.
/// </summary>
/// <param name="Id">The target document's id, or null for no link.</param>
/// <param name="DisplayText">The display text when known.</param>
public sealed record LinkFieldValue(string? Id, string? DisplayText) : FieldValue;

/// <summary>An attach value: a reference to a file record (WP-PLT-10).</summary>
/// <param name="FileId">The file record's id, or null for none.</param>
public sealed record AttachFieldValue(string? FileId) : FieldValue;

/// <summary>A secret field's state: the value itself never leaves the secrets
/// service; the document only carries whether one is set.</summary>
/// <param name="IsSet">True when a secret is set.</param>
public sealed record SecretFieldValue(bool IsSet) : FieldValue;

/// <summary>Base64 bytes, kept as their encoded text (the contract's encoding).</summary>
/// <param name="Base64">The standard base64 with padding.</param>
public sealed record BytesFieldValue(string Base64) : FieldValue;

/// <summary>
/// A JSON value, kept as the JSON text it arrived as (the contract embeds JSON, not a
/// string) and compared semantically here.
/// </summary>
public sealed record JsonFieldValue : FieldValue
{
    /// <summary>Creates a value from the JSON text (any JSON, including null).</summary>
    /// <param name="json">The JSON text.</param>
    public JsonFieldValue(string json)
    {
        JsonText = json;
    }

    /// <summary>The JSON text.</summary>
    public string JsonText { get; init; }

    /// <summary>True when the two texts encode semantically equal JSON.</summary>
    public bool SemanticEquals(JsonFieldValue other)
    {
        using var a = JsonDocument.Parse(JsonText);
        using var b = JsonDocument.Parse(other.JsonText);
        return JsonElementComparer.DeepEqual(a.RootElement, b.RootElement);
    }

    /// <inheritdoc/>
    public override string ToString() => JsonText;
}

/// <summary>Deep equality of JSON elements (RFC 8259 semantic equality: object member
/// order does not matter, array order does, numbers compare as their raw text so no
/// floating point creeps in).</summary>
internal static class JsonElementComparer
{
    /// <summary>Compares two JSON elements semantically.</summary>
    public static bool DeepEqual(JsonElement a, JsonElement b) => (a.ValueKind, b.ValueKind) switch
    {
        (JsonValueKind.Object, JsonValueKind.Object) => ObjectsEqual(a, b),
        (JsonValueKind.Array, JsonValueKind.Array) => ArraysEqual(a, b),
        _ => ScalarEquals(a, b),
    };

    private static bool ObjectsEqual(JsonElement a, JsonElement b)
    {
        if (CountProperties(a) != CountProperties(b))
        {
            return false;
        }

        foreach (var property in a.EnumerateObject())
        {
            if (!b.TryGetProperty(property.Name, out var other))
            {
                return false;
            }

            if (!DeepEqual(property.Value, other))
            {
                return false;
            }
        }

        return true;
    }

    private static bool ArraysEqual(JsonElement a, JsonElement b)
    {
        if (a.GetArrayLength() != b.GetArrayLength())
        {
            return false;
        }

        var ea = a.EnumerateArray().GetEnumerator();
        var eb = b.EnumerateArray().GetEnumerator();
        while (ea.MoveNext() && eb.MoveNext())
        {
            if (!DeepEqual(ea.Current, eb.Current))
            {
                return false;
            }
        }

        return true;
    }

    private static bool ScalarEquals(JsonElement a, JsonElement b)
    {
        if (a.ValueKind != b.ValueKind)
        {
            return false;
        }

        return a.ValueKind switch
        {
            JsonValueKind.String => a.GetString() == b.GetString(),
            JsonValueKind.Number => a.GetRawText() == b.GetRawText(),
            JsonValueKind.True or JsonValueKind.False => true,
            JsonValueKind.Null => true,
            _ => a.GetRawText() == b.GetRawText(),
        };
    }

    private static int CountProperties(JsonElement element)
    {
        var count = 0;
        foreach (var _ in element.EnumerateObject())
        {
            count++;
        }

        return count;
    }
}