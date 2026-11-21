using System.Text;
using System.Text.Json;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects;

/// <summary>
/// Turns an effect's wire value into the metadata core's typed value for one field, with the
/// same rules the server validates with: a decimal is its exact string at the field's scale
/// (more places is refused, never rounded), a select takes one of its listed values, text
/// respects the field's maximum length, and a child table is never set by one effect.
/// Nothing here rounds, approximates or reads a clock (ADR-12).
/// </summary>
public static class EffectValueDecoder
{
    private static readonly JsonDocumentOptions DocumentOptions = new()
    {
        AllowTrailingCommas = false,
        CommentHandling = JsonCommentHandling.Disallow,
    };

    /// <summary>
    /// Decodes <paramref name="valueJson"/> for <paramref name="field"/>. Answers false with
    /// <paramref name="why"/> set when the value is not a value of the field's kind.
    /// </summary>
    public static bool TryDecode(FieldModel field, string valueJson, out FieldValue value, out string why)
    {
        ArgumentNullException.ThrowIfNull(field);
        value = FieldValue.Null;
        why = string.Empty;

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(valueJson, DocumentOptions);
        }
        catch (JsonException)
        {
            why = "the value is not JSON";
            return false;
        }

        using (document)
        {
            return TryDecode(field, document.RootElement, out value, out why);
        }
    }

    /// <summary>Decodes one JSON element for <paramref name="field"/> (see the text overload).</summary>
    public static bool TryDecode(FieldModel field, JsonElement element, out FieldValue value, out string why)
    {
        ArgumentNullException.ThrowIfNull(field);
        value = FieldValue.Null;
        why = string.Empty;

        if (element.ValueKind is JsonValueKind.Null or JsonValueKind.Undefined)
        {
            return true;
        }

        if (field.Kind == FieldKind.Table)
        {
            why = "a child table is not set by one effect";
            return false;
        }

        if (field.Kind is FieldKind.Secret)
        {
            why = "a secret is set through the secrets service, never by an effect";
            return false;
        }

        if (field.Kind is FieldKind.Unknown)
        {
            why = "this field's kind is newer than this client";
            return false;
        }

        if (field.Kind == FieldKind.Json)
        {
            value = new JsonFieldValue(element.GetRawText());
            return true;
        }

        if (field.Kind == FieldKind.Bytes)
        {
            if (element.ValueKind != JsonValueKind.String)
            {
                why = "bytes travel as standard base64 text";
                return false;
            }

            value = new BytesFieldValue(element.GetString() ?? string.Empty);
            return true;
        }

        if (field.Multiple && field.Kind == FieldKind.Select)
        {
            if (element.ValueKind != JsonValueKind.Array)
            {
                why = "a multiple select takes an array of listed values";
                return false;
            }

            var selected = new List<string>();
            foreach (JsonElement item in element.EnumerateArray())
            {
                if (item.ValueKind != JsonValueKind.String)
                {
                    why = "a multiple select takes an array of strings";
                    return false;
                }

                string text = item.GetString() ?? string.Empty;
                if (!field.Options.Contains(text, StringComparer.Ordinal))
                {
                    why = "a value is not one of the select's values";
                    return false;
                }

                selected.Add(text);
            }

            value = new MultiSelectFieldValue(Array.AsReadOnly(selected.ToArray()));
            return true;
        }

        // The wire form of each kind: a decimal and an int may arrive as a JSON number or the
        // contract's string, a bool as a JSON bool, and every other scalar kind as a string.
        switch (field.Kind)
        {
            case FieldKind.Bool when element.ValueKind is not (JsonValueKind.True or JsonValueKind.False):
                why = "a bool field takes true or false";
                return false;
            case FieldKind.IntegerNumber or FieldKind.DecimalNumber
                when element.ValueKind is not (JsonValueKind.Number or JsonValueKind.String):
                why = "a number arrives as a JSON number or the contract's string";
                return false;
            case FieldKind.Text or FieldKind.LongText or FieldKind.Select or FieldKind.Date
                or FieldKind.Time or FieldKind.Datetime or FieldKind.Link or FieldKind.DynamicLink
                or FieldKind.Attach
                when element.ValueKind != JsonValueKind.String:
                why = "this field's value travels as a string";
                return false;
            default:
                break;
        }

        string? scalar = ScalarText(element, out why);
        if (scalar is null)
        {
            return false;
        }

        if (field.Kind is FieldKind.Text or FieldKind.LongText)
        {
            if (field.MaxChars is { } max && CountRunes(scalar) > max)
            {
                why = $"the text is longer than {max} characters";
                return false;
            }
        }

        if (!field.TryParse(scalar, out FieldValue? parsed, out FieldError? error) || parsed is null)
        {
            why = error?.Message ?? "the value does not fit the field's kind";
            return false;
        }

        value = parsed;
        return true;
    }

    private static string? ScalarText(JsonElement element, out string why)
    {
        why = string.Empty;
        switch (element.ValueKind)
        {
            case JsonValueKind.String:
                return element.GetString() ?? string.Empty;
            case JsonValueKind.Number:
                return element.GetRawText();
            case JsonValueKind.True:
                return "true";
            case JsonValueKind.False:
                return "false";
            default:
                why = "a value of this kind is a single scalar";
                return null;
        }
    }

    private static int CountRunes(string text)
    {
        int count = 0;
        foreach (Rune _ in text.EnumerateRunes())
        {
            count++;
        }

        return count;
    }
}
