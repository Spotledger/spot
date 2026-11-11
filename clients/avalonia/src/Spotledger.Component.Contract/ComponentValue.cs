using System.Globalization;
using System.Text.Json;

namespace Spotledger.Component.Contract;

/// <summary>
/// One value flowing between host and component, carrying its field type. The payload is the
/// canonical wire text (the same encodings the metadata contract uses) so a value can cross a
/// serialisation boundary unchanged. Construct values through the named factories; that keeps
/// the encoding rules in one place and refuses a mismatched pair early.
/// <para>
/// There is no value at all (<see cref="IsNull"/>) and there is the type's empty value
/// (<see cref="IsEmpty"/>): a required field must reject both, a component must round-trip both.
/// </para>
/// </summary>
public readonly record struct ComponentValue
{
    private ComponentValue(ComponentFieldType type, string? text)
    {
        Type = type;
        Text = text;
    }

    /// <summary>The field type this value belongs to.</summary>
    public ComponentFieldType Type { get; }

    /// <summary>The canonical wire text; null when there is no value.</summary>
    public string? Text { get; }

    /// <summary>True when there is no value at all.</summary>
    public bool IsNull => Text is null;

    /// <summary>True when the value carries nothing: null, empty text, or a zero-shaped number.</summary>
    public bool IsEmpty => Text is null || Text.Length == 0;

    /// <summary>A value of <paramref name="type"/> with nothing in it.</summary>
    public static ComponentValue Null(ComponentFieldType type) => new(type, null);

    /// <summary>A boolean value.</summary>
    public static ComponentValue FromBoolean(bool value) => new(ComponentFieldType.Boolean, value ? "true" : "false");

    /// <summary>An integer value.</summary>
    public static ComponentValue FromInteger(long value) =>
        new(ComponentFieldType.IntegerNumber, value.ToString(CultureInfo.InvariantCulture));

    /// <summary>A decimal value, formatted invariantly and exactly.</summary>
    public static ComponentValue FromDecimal(decimal value) =>
        new(ComponentFieldType.DecimalNumber, value.ToString(CultureInfo.InvariantCulture));

    /// <summary>A single-line or multi-line text value.</summary>
    public static ComponentValue FromText(string value) =>
        new(ComponentFieldType.Text, value ?? throw new ArgumentNullException(nameof(value)));

    /// <summary>A text value of the given string-shaped field type.</summary>
    public static ComponentValue FromString(ComponentFieldType type, string value)
    {
        if (type is ComponentFieldType.Boolean or ComponentFieldType.IntegerNumber or ComponentFieldType.DecimalNumber
            or ComponentFieldType.Date or ComponentFieldType.Time or ComponentFieldType.Datetime)
        {
            throw new ArgumentException("Use the typed factory for " + type.ToWireName() + ".", nameof(type));
        }

        return new ComponentValue(type, value ?? throw new ArgumentNullException(nameof(value)));
    }

    /// <summary>Long text, encoded exactly like text.</summary>
    public static ComponentValue FromLongText(string value) =>
        new(ComponentFieldType.LongText, value ?? throw new ArgumentNullException(nameof(value)));

    /// <summary>A calendar date.</summary>
    public static ComponentValue FromDate(DateOnly value) =>
        new(ComponentFieldType.Date, value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture));

    /// <summary>A wall-clock time, never a clock reading.</summary>
    public static ComponentValue FromTime(TimeOnly value) =>
        new(ComponentFieldType.Time, value.ToString("HH:mm:ss", CultureInfo.InvariantCulture));

    /// <summary>An instant: a value the host supplies, never <c>Now</c>.</summary>
    public static ComponentValue FromInstant(DateTimeOffset value) =>
        new(ComponentFieldType.Datetime, value.ToString("O", CultureInfo.InvariantCulture));

    /// <summary>A link: the document id, with the display text kept beside it.</summary>
    public static ComponentValue FromLink(string id, string? displayText = null) =>
        new(ComponentFieldType.Link, displayText is null ? id : id + "\u001f" + displayText);

    /// <summary>The id part of a <see cref="ComponentFieldType.Link"/> value.</summary>
    public string? LinkId => Type == ComponentFieldType.Link && Text is not null ? SplitLink(Text)[0] : null;

    /// <summary>The display-text part of a <see cref="ComponentFieldType.Link"/> value.</summary>
    public string? LinkDisplayText
    {
        get
        {
            if (Type != ComponentFieldType.Link || Text is null)
            {
                return null;
            }

            var parts = SplitLink(Text);
            return parts.Length > 1 ? parts[1] : null;
        }
    }

    /// <summary>A JSON text value, verified to parse.</summary>
    public static ComponentValue FromJson(string json)
    {
        ArgumentNullException.ThrowIfNull(json);
        using var document = JsonDocument.Parse(json);
        return new ComponentValue(ComponentFieldType.Json, document.RootElement.GetRawText());
    }

    /// <summary>A byte payload, carried as base64.</summary>
    public static ComponentValue FromBytes(ReadOnlySpan<byte> bytes) =>
        new(ComponentFieldType.Bytes, Convert.ToBase64String(bytes));

    /// <summary>The type's empty value: zero, empty text, an empty table.</summary>
    public static ComponentValue Empty(ComponentFieldType type) => type switch
    {
        ComponentFieldType.Boolean => FromBoolean(false),
        ComponentFieldType.IntegerNumber => FromInteger(0),
        ComponentFieldType.DecimalNumber => FromDecimal(decimal.Zero),
        ComponentFieldType.Date or ComponentFieldType.Time or ComponentFieldType.Datetime
            or ComponentFieldType.Link or ComponentFieldType.Attach or ComponentFieldType.Secret
            or ComponentFieldType.Bytes => new ComponentValue(type, string.Empty),
        ComponentFieldType.Table => new ComponentValue(ComponentFieldType.Table, "[]"),
        ComponentFieldType.Json => new ComponentValue(ComponentFieldType.Json, "{}"),
        _ => new ComponentValue(type, string.Empty),
    };

    /// <summary>The value as a boolean, when it is one.</summary>
    public bool TryGetBoolean(out bool value)
    {
        value = false;
        return Type == ComponentFieldType.Boolean && Text is not null
            && bool.TryParse(Text, out value);
    }

    /// <summary>The value as an integer, when it is one.</summary>
    public bool TryGetInteger(out long value)
    {
        value = 0;
        return Type == ComponentFieldType.IntegerNumber && Text is not null
            && long.TryParse(Text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out value);
    }

    /// <summary>The value as an exact decimal, when it is one (never through a float).</summary>
    public bool TryGetDecimal(out decimal value)
    {
        value = decimal.Zero;
        return Type == ComponentFieldType.DecimalNumber && Text is not null
            && decimal.TryParse(Text, NumberStyles.Number, CultureInfo.InvariantCulture, out value);
    }

    /// <summary>The value as a calendar date, when it is one.</summary>
    public bool TryGetDate(out DateOnly value)
    {
        value = default;
        return Type == ComponentFieldType.Date && Text is not null
            && DateOnly.TryParseExact(Text, "yyyy-MM-dd", CultureInfo.InvariantCulture, DateTimeStyles.None, out value);
    }

    /// <summary>The value as a wall-clock time, when it is one.</summary>
    public bool TryGetTime(out TimeOnly value)
    {
        value = default;
        return Type == ComponentFieldType.Time && Text is not null
            && TimeOnly.TryParseExact(Text, "HH:mm:ss", CultureInfo.InvariantCulture, DateTimeStyles.None, out value);
    }

    /// <summary>The value as an instant, when it is one.</summary>
    public bool TryGetInstant(out DateTimeOffset value)
    {
        value = default;
        return Type == ComponentFieldType.Datetime && Text is not null
            && DateTimeOffset.TryParse(Text, CultureInfo.InvariantCulture, DateTimeStyles.RoundtripKind, out value);
    }

    /// <inheritdoc />
    public override string ToString() => Text is null
        ? Type.ToWireName() + ":null"
        : Type == ComponentFieldType.Secret
            ? Type.ToWireName() + ":" + (Text.Length == 0 ? "unset" : "set")
            : Type.ToWireName() + ":" + Text;

    private static string[] SplitLink(string text) => text.Split('\u001f');
}
