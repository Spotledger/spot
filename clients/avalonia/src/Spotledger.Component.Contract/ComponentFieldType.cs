namespace Spotledger.Component.Contract;

/// <summary>
/// The field kinds a component may bind to. Mirrors the client metadata vocabulary
/// (<c>FieldKind</c>) by name without referencing it, so the contract package depends on no
/// application code: a host adapts its field metadata to <see cref="FieldMetadata"/>.
/// Money and quantities are always <see cref="Decimal"/> (ADR-12: never a float).
/// </summary>
public enum ComponentFieldType
{
    /// <summary>A true/false value.</summary>
    Boolean,

    /// <summary>A 64-bit signed integer.</summary>
    IntegerNumber,

    /// <summary>An exact decimal: money, quantities and rates.</summary>
    DecimalNumber,

    /// <summary>A single-line text value.</summary>
    Text,

    /// <summary>A multi-line text value.</summary>
    LongText,

    /// <summary>A calendar date (<c>YYYY-MM-DD</c>).</summary>
    Date,

    /// <summary>A wall-clock time (<c>HH:MM:SS</c>).</summary>
    Time,

    /// <summary>An instant, carried as <see cref="DateTimeOffset"/>.</summary>
    Datetime,

    /// <summary>A reference to another document: an id plus optional display text.</summary>
    Link,

    /// <summary>One of a listed set of options.</summary>
    Select,

    /// <summary>Arbitrary JSON kept as text.</summary>
    Json,

    /// <summary>Base64 bytes.</summary>
    Bytes,

    /// <summary>A file record id, never the bytes.</summary>
    Attach,

    /// <summary>A secret: only whether one is set is ever visible.</summary>
    Secret,

    /// <summary>A child table, carried as JSON rows.</summary>
    Table,
}

/// <summary>Name mapping for <see cref="ComponentFieldType"/>: the encoding names on the wire.</summary>
public static class ComponentFieldTypes
{
    /// <summary>
    /// Every field type the contract knows, in declaration order. A descriptor lists the
    /// subset it can render; the kit checks the value round trip for exactly that subset.
    /// </summary>
    public static IReadOnlyList<ComponentFieldType> All { get; } = new[]
    {
        ComponentFieldType.Boolean,
        ComponentFieldType.IntegerNumber,
        ComponentFieldType.DecimalNumber,
        ComponentFieldType.Text,
        ComponentFieldType.LongText,
        ComponentFieldType.Date,
        ComponentFieldType.Time,
        ComponentFieldType.Datetime,
        ComponentFieldType.Link,
        ComponentFieldType.Select,
        ComponentFieldType.Json,
        ComponentFieldType.Bytes,
        ComponentFieldType.Attach,
        ComponentFieldType.Secret,
        ComponentFieldType.Table,
    };

    /// <summary>The wire name of a field type.</summary>
    public static string ToWireName(this ComponentFieldType type) => type switch
    {
        ComponentFieldType.Boolean => "bool",
        ComponentFieldType.IntegerNumber => "int",
        ComponentFieldType.DecimalNumber => "decimal",
        ComponentFieldType.Text => "text",
        ComponentFieldType.LongText => "long_text",
        ComponentFieldType.Date => "date",
        ComponentFieldType.Time => "time",
        ComponentFieldType.Datetime => "datetime",
        ComponentFieldType.Link => "link",
        ComponentFieldType.Select => "select",
        ComponentFieldType.Json => "json",
        ComponentFieldType.Bytes => "bytes",
        ComponentFieldType.Attach => "attach",
        ComponentFieldType.Secret => "secret",
        ComponentFieldType.Table => "table",
        _ => throw new ArgumentOutOfRangeException(nameof(type), type, "Unknown field type."),
    };

    /// <summary>Maps a wire name back to its field type; null when the name is unknown.</summary>
    public static ComponentFieldType? TryParse(string? wireName)
    {
        if (string.IsNullOrWhiteSpace(wireName))
        {
            return null;
        }

        foreach (var type in All)
        {
            if (string.Equals(type.ToWireName(), wireName, StringComparison.Ordinal))
            {
                return type;
            }
        }

        return null;
    }
}
