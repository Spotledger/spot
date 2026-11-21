using System.Globalization;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

/// <summary>One column of the list: the field, whether the person reads it masked, and the grid model.</summary>
public sealed class ListColumn(FieldModel field, bool masked, GridColumnModel model)
{
    /// <summary>The field.</summary>
    public FieldModel Field { get; } = field;

    /// <summary>True when the server sends this field masked to this person.</summary>
    public bool Masked { get; } = masked;

    /// <summary>The grid's column model (header, width, alignment).</summary>
    public GridColumnModel Model { get; } = model;
}

/// <summary>Which fields a list may show and what it shows by default. A field the permission
/// summary hides, or the metadata marks hidden, is never offered: the list fails closed.</summary>
public static class ListColumnPlanner
{
    /// <summary>The fields this person may have as columns, in definition order.</summary>
    public static IReadOnlyList<FieldModel> Readable(DocTypeModel model, DocTypePermissionState? permissions)
    {
        if (permissions is null)
        {
            return [];
        }

        var hidden = new HashSet<string>(permissions.Hidden, StringComparer.Ordinal);
        return model.Fields
            .Where(f => !hidden.Contains(f.Name) && !f.Hints.Hidden && IsDisplayable(f.Kind) && ListLimits.IsIdentifier(f.Name))
            .ToArray();
    }

    /// <summary>True for kinds a cell can show.</summary>
    public static bool IsDisplayable(FieldKind kind) => kind is FieldKind.Bool or FieldKind.IntegerNumber or FieldKind.DecimalNumber
        or FieldKind.Text or FieldKind.LongText or FieldKind.Date or FieldKind.Time or FieldKind.Datetime
        or FieldKind.Link or FieldKind.DynamicLink or FieldKind.Select;

    /// <summary>The view's default columns: the fields marked for the list, else the first few readable ones.</summary>
    public static IReadOnlyList<FieldModel> Defaults(IReadOnlyList<FieldModel> readable)
    {
        var marked = readable.Where(f => f.InListView).Take(12).ToArray();
        return marked.Length > 0 ? marked : readable.Where(f => f.Kind != FieldKind.LongText).Take(6).ToArray();
    }

    /// <summary>The fields the quick search looks in: the text fields among the readable ones that the
    /// list shows, at most five (the contract has no search-field list for a view yet).</summary>
    public static IReadOnlyList<FieldModel> SearchFields(IReadOnlyList<FieldModel> readable, IReadOnlyList<FieldModel> shown, DocTypePermissionState? permissions)
    {
        var masked = new HashSet<string>(permissions?.Masked ?? [], StringComparer.Ordinal);
        return shown.Where(f => f.Kind == FieldKind.Text && !masked.Contains(f.Name) && readable.Contains(f)).Take(5).ToArray();
    }

    /// <summary>Applies the person's saved layout to the defaults: unknown or unreadable fields are dropped.</summary>
    public static IReadOnlyList<(FieldModel Field, double? Width)> Layout(
        IReadOnlyList<FieldModel> readable, IReadOnlyList<FieldModel> defaults, IReadOnlyList<ColumnPreference>? saved)
    {
        if (saved is null || saved.Count == 0)
        {
            return defaults.Select(f => (f, (double?)null)).ToArray();
        }

        var byName = readable.ToDictionary(f => f.Name, StringComparer.Ordinal);
        var result = saved.Where(c => byName.ContainsKey(c.Field)).Select(c => (byName[c.Field], c.Width)).ToArray();
        return result.Length > 0 ? result : defaults.Select(f => (f, (double?)null)).ToArray();
    }

    /// <summary>Builds the columns for a layout.</summary>
    public static IReadOnlyList<ListColumn> Build(IEnumerable<(FieldModel Field, double? Width)> layout, DocTypePermissionState? permissions)
    {
        var masked = new HashSet<string>(permissions?.Masked ?? [], StringComparer.Ordinal);
        return layout.Select(item =>
        {
            var field = item.Field;
            var model = new GridColumnModel
            {
                Header = field.Label,
                Width = item.Width is { } w ? GridColumnWidth.Pixels(w) : DefaultWidth(field),
                DefaultAlignment = Align(field),
                IsSortable = field.Kind is not FieldKind.LongText && !masked.Contains(field.Name),
            };
            return new ListColumn(field, masked.Contains(field.Name), model);
        }).ToArray();
    }

    internal static GridCellAlignment Align(FieldModel field) => field.Kind switch
    {
        FieldKind.DecimalNumber or FieldKind.IntegerNumber => GridCellAlignment.End,
        FieldKind.Bool => GridCellAlignment.Center,
        _ => GridCellAlignment.Start,
    };

    internal static GridColumnWidth DefaultWidth(FieldModel field) => field.Kind switch
    {
        FieldKind.DecimalNumber or FieldKind.IntegerNumber => GridColumnWidth.Pixels(120),
        FieldKind.Date => GridColumnWidth.Pixels(120),
        FieldKind.Time => GridColumnWidth.Pixels(100),
        FieldKind.Datetime => GridColumnWidth.Pixels(170),
        FieldKind.Bool => GridColumnWidth.Pixels(80),
        FieldKind.Text or FieldKind.LongText or FieldKind.Link or FieldKind.DynamicLink => GridColumnWidth.Star(2),
        _ => GridColumnWidth.Star(1),
    };
}

/// <summary>Turns decoded server values into display strings: one place, so the grid, the status
/// line and the client-side export agree. Text is data: control characters become spaces, nothing is
/// interpreted as markup.</summary>
public sealed class ListCellFormatter(CultureInfo? culture = null, TimeZoneInfo? zone = null)
{
    /// <summary>What a masked value with no text of its own shows.</summary>
    public const string MaskText = "••••";

    private const int MaxCellChars = 500;
    private readonly CultureInfo _culture = culture ?? CultureInfo.CurrentCulture;
    private readonly TimeZoneInfo _zone = zone ?? TimeZoneInfo.Local;

    /// <summary>Formats one cell.</summary>
    public GridCell Format(ListColumn column, JsonElement? value, IReadOnlyDictionary<string, string>? labels)
    {
        var field = column.Field;
        var alignment = ListColumnPlanner.Align(field);
        if (column.Masked)
        {
            // The server forms the mask; the client never formats it as the kind it hides.
            var raw = value is { ValueKind: JsonValueKind.String } element ? element.GetString() : null;
            return new GridCell(string.IsNullOrEmpty(raw) ? MaskText : Clean(raw), alignment, "masked");
        }

        if (value is not { } json || json.ValueKind is JsonValueKind.Null or JsonValueKind.Undefined)
        {
            return new GridCell(string.Empty, alignment);
        }

        return new GridCell(Clean(Text(field, json, labels)), alignment);
    }

    private string Text(FieldModel field, JsonElement json, IReadOnlyDictionary<string, string>? labels)
    {
        switch (field.Kind)
        {
            case FieldKind.Bool:
                return json.ValueKind switch { JsonValueKind.True => "Yes", JsonValueKind.False => "No", _ => Raw(json) };
            case FieldKind.IntegerNumber:
                if (json.ValueKind == JsonValueKind.Number && json.TryGetInt64(out var whole))
                {
                    return whole.ToString("N0", _culture);
                }

                return json.ValueKind == JsonValueKind.String && long.TryParse(json.GetString(), NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var parsed)
                    ? parsed.ToString("N0", _culture)
                    : Raw(json);
            case FieldKind.DecimalNumber:
                return json.ValueKind == JsonValueKind.String ? Decimal(json.GetString()!, field.Scale ?? 0) : Raw(json);
            case FieldKind.Date:
                return json.ValueKind == JsonValueKind.String && TemporalFormat.TryParseDate(json.GetString()!, out var date)
                    ? date.ToString("d", _culture)
                    : Raw(json);
            case FieldKind.Time:
                return json.ValueKind == JsonValueKind.String && TemporalFormat.TryParseTime(json.GetString()!, out var time)
                    ? time.ToString("t", _culture)
                    : Raw(json);
            case FieldKind.Datetime:
                return json.ValueKind == JsonValueKind.String && TemporalFormat.TryParseDateTime(json.GetString()!, out var moment)
                    ? TimeZoneInfo.ConvertTime(moment, _zone).ToString("g", _culture)
                    : Raw(json);
            case FieldKind.Link:
                var id = json.ValueKind == JsonValueKind.String ? json.GetString()! : Raw(json);
                return labels is not null && labels.TryGetValue(id, out var label) ? label : id;
            case FieldKind.Select when json.ValueKind == JsonValueKind.Array:
                return string.Join(", ", json.EnumerateArray().Select(Raw));
            default:
                return Raw(json);
        }
    }

    private static string Raw(JsonElement json) => json.ValueKind == JsonValueKind.String ? json.GetString() ?? string.Empty : json.GetRawText();

    /// <summary>Groups the integer digits and swaps the separator by culture, padding to <paramref name="scale"/>.
    /// Exact: no digit is rounded or converted through a floating-point type.</summary>
    internal string Decimal(string canonical, int scale)
    {
        var text = canonical;
        var negative = text.StartsWith('-');
        if (negative)
        {
            text = text[1..];
        }

        var dot = text.IndexOf('.', StringComparison.Ordinal);
        var whole = dot < 0 ? text : text[..dot];
        var fraction = dot < 0 ? string.Empty : text[(dot + 1)..];
        if (whole.Length == 0 || !whole.All(char.IsAsciiDigit) || !fraction.All(char.IsAsciiDigit))
        {
            return canonical;
        }

        fraction = fraction.PadRight(Math.Max(scale, fraction.Length), '0');
        var separator = _culture.NumberFormat.NumberGroupSeparator;
        var builder = new StringBuilder();
        for (var i = 0; i < whole.Length; i++)
        {
            if (i > 0 && (whole.Length - i) % 3 == 0)
            {
                builder.Append(separator);
            }

            builder.Append(whole[i]);
        }

        if (fraction.Length > 0)
        {
            builder.Append(_culture.NumberFormat.NumberDecimalSeparator).Append(fraction);
        }

        return (negative ? _culture.NumberFormat.NegativeSign : string.Empty) + builder;
    }

    private static string Clean(string text)
    {
        if (text.Length > MaxCellChars)
        {
            text = text[..MaxCellChars];
        }

        foreach (var c in text)
        {
            if (char.IsControl(c))
            {
                return string.Create(text.Length, text, static (span, source) =>
                {
                    for (var i = 0; i < source.Length; i++)
                    {
                        span[i] = char.IsControl(source[i]) ? ' ' : source[i];
                    }
                });
            }
        }

        return text;
    }
}
