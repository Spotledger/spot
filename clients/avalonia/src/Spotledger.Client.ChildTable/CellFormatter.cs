using System.Globalization;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.ChildTable;

/// <summary>Formats a row value as the display string the grid draws. Decimals keep their
/// exact digits (padded to the field's scale, never rounded); the text is also what a copy
/// puts on the clipboard, so a paste parses it back to the same value.</summary>
internal static class CellFormatter
{
    public static string Format(FieldModel field, FieldValue? value)
    {
        switch (value)
        {
            case null:
            case NullFieldValue:
                return string.Empty;
            case DecimalFieldValue number:
                return PadScale(number.CanonicalText, field.Scale ?? 0);
            case IntFieldValue integer:
                return integer.Value.ToString(CultureInfo.InvariantCulture);
            case BoolFieldValue flag:
                return flag.Value ? "true" : "false";
            case TextFieldValue text:
                return text.Value;
            case SelectFieldValue option:
                return option.Value;
            case MultiSelectFieldValue options:
                return string.Join(", ", options.Values);
            case DateFieldValue date:
                return date.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture);
            case TimeFieldValue time:
                return time.Value.ToString("HH:mm:ss", CultureInfo.InvariantCulture);
            case DateTimeFieldValue instant:
                return instant.Value.ToString("O", CultureInfo.InvariantCulture);
            case LinkFieldValue link:
                return link.DisplayText ?? link.Id ?? string.Empty;
            case AttachFieldValue file:
                return file.FileId ?? string.Empty;
            case SecretFieldValue secret:
                return secret.IsSet ? "Set" : string.Empty;
            case BytesFieldValue bytes:
                return bytes.Base64;
            case JsonFieldValue json:
                return json.JsonText;
            default:
                return string.Empty;
        }
    }

    public static GridCellAlignment Align(FieldModel field) => field.Kind switch
    {
        FieldKind.DecimalNumber or FieldKind.IntegerNumber => GridCellAlignment.End,
        FieldKind.Bool => GridCellAlignment.Center,
        _ => GridCellAlignment.Start,
    };

    public static GridColumnWidth DefaultWidth(FieldModel field) => field.Kind switch
    {
        FieldKind.DecimalNumber or FieldKind.IntegerNumber => GridColumnWidth.Pixels(110),
        FieldKind.Date => GridColumnWidth.Pixels(120),
        FieldKind.Time => GridColumnWidth.Pixels(100),
        FieldKind.Datetime => GridColumnWidth.Pixels(190),
        FieldKind.Bool => GridColumnWidth.Pixels(72),
        FieldKind.Text or FieldKind.LongText or FieldKind.Link or FieldKind.DynamicLink => GridColumnWidth.Star(2),
        _ => GridColumnWidth.Star(1),
    };

    /// <summary>Pads the fraction of canonical decimal text with zeros to at least
    /// <paramref name="scale"/> digits. It never removes a digit.</summary>
    internal static string PadScale(string canonical, int scale)
    {
        if (scale <= 0)
        {
            return canonical;
        }

        var dot = canonical.IndexOf('.', StringComparison.Ordinal);
        var have = dot < 0 ? 0 : canonical.Length - dot - 1;
        if (have >= scale)
        {
            return canonical;
        }

        return dot < 0
            ? canonical + "." + new string('0', scale)
            : canonical + new string('0', scale - have);
    }
}
