using System.Globalization;

namespace Spotledger.Client.Effects;

/// <summary>
/// A field path in the form the effects protocol uses: <c>field</c> or
/// <c>table[idx].field</c>, with <c>idx</c> counting from 1 — the same form the
/// form-event endpoint takes (<c>crates/framework/spot-form-effects/src/path.rs</c>).
/// Whether the field exists is the metadata's question, asked after this syntax check.
/// </summary>
public readonly record struct EffectPath
{
    private EffectPath(string? table, int? index, string field)
    {
        Table = table;
        Index = index;
        Field = field;
    }

    /// <summary>The child table, when the path names a row.</summary>
    public string? Table { get; }

    /// <summary>The row's 1-based position, when the path names a row.</summary>
    public int? Index { get; }

    /// <summary>The field: a top-level field, or a column of the row's child DocType.</summary>
    public string Field { get; }

    /// <summary>Whether the path names a row of a child table.</summary>
    public bool IsRow => Table is not null;

    /// <summary>A top-level field path.</summary>
    public static EffectPath OfField(string field) => new(null, null, field);

    /// <summary>A row column path (<paramref name="index"/> from 1).</summary>
    public static EffectPath OfRow(string table, int index, string field) => new(table, index, field);

    /// <summary>
    /// Parses the wire form: <c>field</c> or <c>table[idx].field</c>. Answers false for an
    /// empty text, a name with a bracket, dot or whitespace, a non-numeric or zero index,
    /// or an extra <c>[</c>.
    /// </summary>
    public static bool TryParse(string? text, out EffectPath path)
    {
        path = default;
        if (string.IsNullOrEmpty(text))
        {
            return false;
        }

        int open = text.IndexOf('[', StringComparison.Ordinal);
        if (open < 0)
        {
            if (!IsName(text))
            {
                return false;
            }

            path = OfField(text);
            return true;
        }

        string table = text[..open];
        if (!IsName(table))
        {
            return false;
        }

        string rest = text[(open + 1)..];
        int close = rest.IndexOf("].", StringComparison.Ordinal);
        if (close < 0)
        {
            return false;
        }

        string digits = rest[..close];
        string field = rest[(close + 2)..];
        if (!IsName(field)
            || !int.TryParse(digits, NumberStyles.None, CultureInfo.InvariantCulture, out int index)
            || index <= 0)
        {
            return false;
        }

        path = OfRow(table, index, field);
        return true;
    }

    /// <summary>The path as the wire writes it (the 1-based index when there is one).</summary>
    public string Wire() => IsRow
        ? string.Create(CultureInfo.InvariantCulture, $"{Table}[{Index}].{Field}")
        : Field;

    /// <inheritdoc/>
    public override string ToString() => Wire();

    private static bool IsName(string text)
    {
        if (text.Length == 0)
        {
            return false;
        }

        foreach (char c in text)
        {
            if (c is '[' or ']' or '.' || char.IsWhiteSpace(c))
            {
                return false;
            }
        }

        return true;
    }
}
