using System.Text.Json;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Metadata;

/// <summary>
/// Decodes the contract's <c>FieldValue</c> JSON encoding into the typed
/// <see cref="FieldValue"/> models and encodes them back: decimals and large integers
/// arrive as strings and leave as strings, JSON is embedded rather than quoted, and a
/// child table is an array of row objects. Which encoding applies is decided by the
/// field's metadata, never by the JSON itself. No reflection, no dynamic: JSON is read
/// with <see cref="JsonDocument"/> and written with <see cref="Utf8JsonWriter"/>,
/// which keeps the code AOT and trim safe.
/// </summary>
public static class ValueCodec
{
    /// <summary>The JSON options the codec uses: strict, no comments.</summary>
    private static readonly JsonDocumentOptions DocumentOptions = new()
    {
        AllowTrailingCommas = false,
        CommentHandling = JsonCommentHandling.Disallow,
    };

    /// <summary>
    /// Decodes the generated <c>FieldValue</c> (a field's default in the metadata) by
    /// the field's kind. A default of no usable shape falls back to the kind's empty
    /// value rather than throwing.
    /// </summary>
    public static FieldValue DecodeDefault(Spotledger.Client.Api.Generated.Models.FieldValue raw, FieldKind kind)
    {
        if (raw is null)
        {
            return kind.EmptyValue();
        }

        if (raw.String is { } text)
        {
            return DecodeString(text, kind);
        }

        if (raw.Integer is { } integer)
        {
            return kind switch
            {
                FieldKind.IntegerNumber => new IntFieldValue(integer),
                FieldKind.DecimalNumber => new DecimalFieldValue(new decimal(integer)),
                _ => kind.EmptyValue(),
            };
        }

        if (raw.Boolean is { } boolean)
        {
            return kind == FieldKind.Bool ? new BoolFieldValue(boolean) : kind.EmptyValue();
        }

        return kind.EmptyValue();
    }

    /// <summary>Decodes a JSON <c>JsonElement</c> by the field's kind.</summary>
    public static FieldValue DecodeElement(JsonElement element, FieldKind kind)
    {
        switch (element.ValueKind)
        {
            case JsonValueKind.Null or JsonValueKind.Undefined:
                return FieldValue.Null;
            case JsonValueKind.String:
                return DecodeString(element.GetString() ?? string.Empty, kind);
            case JsonValueKind.Number:
                // The contract never encodes a decimal as a JSON number, and a JSON
                // number for a decimal field is rejected server-side. An int arrives as
                // a genuine JSON integer.
                if (element.TryGetInt64(out var integer))
                {
                    return new IntFieldValue(integer);
                }

                // A decimal-encoded-as-number can only be a contract violation; keep
                // the raw text as data rather than losing it or going through a float.
                return new TextFieldValue(element.GetRawText());
            case JsonValueKind.True:
                return new BoolFieldValue(true);
            case JsonValueKind.False:
                return new BoolFieldValue(false);
            case JsonValueKind.Array when kind == FieldKind.Table:
                return DecodeTable(element, null);
            case JsonValueKind.Object:
            case JsonValueKind.Array:
                return new JsonFieldValue(element.GetRawText());
            default:
                return FieldValue.Null;
        }
    }

    /// <summary>Decodes with the whole field model, including multiple-select arrays.</summary>
    public static FieldValue DecodeElement(JsonElement element, FieldModel field)
    {
        ArgumentNullException.ThrowIfNull(field);
        if (field.Multiple && element.ValueKind == JsonValueKind.Array)
        {
            return new MultiSelectFieldValue(Array.AsReadOnly(element.EnumerateArray().Select(item => item.GetString()!).ToArray()));
        }
        return DecodeElement(element, field.Kind);
    }

    private static FieldValue DecodeString(string text, FieldKind kind) => kind switch
    {
        FieldKind.DecimalNumber => DecodeDecimalString(text),
        FieldKind.IntegerNumber => DecodeIntString(text),
        FieldKind.Date => TemporalFormat.TryParseDate(text, out var date) ? new DateFieldValue(date) : new TextFieldValue(text),
        FieldKind.Time => TemporalFormat.TryParseTime(text, out var time) ? new TimeFieldValue(time) : new TextFieldValue(text),
        FieldKind.Datetime => TemporalFormat.TryParseDateTime(text, out var moment) ? new DateTimeFieldValue(moment) : new TextFieldValue(text),
        FieldKind.Bool => text == "true" ? new BoolFieldValue(true) : text == "false" ? new BoolFieldValue(false) : new TextFieldValue(text),
        FieldKind.Select => new SelectFieldValue(text),
        FieldKind.Link or FieldKind.DynamicLink => new LinkFieldValue(text, null),
        FieldKind.Attach => new AttachFieldValue(text),
        FieldKind.Bytes => new BytesFieldValue(text),
        FieldKind.Secret => new SecretFieldValue(true),
        FieldKind.Json => new JsonFieldValue(text),
        _ => new TextFieldValue(text),
    };

    /// <summary>Decodes the contract's decimal string exactly: a <see cref="System.Decimal"/>
    /// when the digits fit 28 significant ones, the string-backed value otherwise.</summary>
    public static FieldValue DecodeDecimalString(string text)
    {
        var exact = ExactDecimal.FromCanonical(text);
        return new DecimalFieldValue(exact);
    }

    /// <summary>Decodes the contract's integer (from its string encoding when large, or
    /// a genuine JSON integer).</summary>
    public static FieldValue DecodeIntString(string text)
    {
        if (long.TryParse(text, out var parsed))
        {
            return new IntFieldValue(parsed);
        }

        // A value beyond 64 bits would be a contract violation; keep it as text data
        // rather than losing information or crashing.
        return new TextFieldValue(text);
    }

    /// <summary>Decodes a document's child-table JSON array into rows.</summary>
    public static TableFieldValue DecodeTable(JsonElement array, DocTypeModel? rowModel, Func<int, string, bool>? isMasked = null)
    {
        var rows = new List<TableRowValue>();
        foreach (var row in array.EnumerateArray())
        {
            if (row.ValueKind != JsonValueKind.Object)
            {
                continue;
            }

            string? id = null;
            var values = new Dictionary<string, FieldValue>(StringComparer.Ordinal);
            foreach (var property in row.EnumerateObject())
            {
                if (property.Name == "id")
                {
                    id = property.Value.ValueKind == JsonValueKind.String ? property.Value.GetString() : null;
                    continue;
                }

                if (property.Name == "idx")
                {
                    // Position comes from the array order (the contract's idx follows it).
                    continue;
                }

                // The row's field kind comes from the row model when it is loaded (a
                // decimal row value arrives as the contract's decimal string); without
                // it the raw JSON decodes by its own shape as text.
                if (isMasked?.Invoke(rows.Count + 1, property.Name) == true)
                {
                    values[property.Name] = FieldValue.Null;
                    continue;
                }
                var rowField = rowModel?.Field(property.Name);
                values[property.Name] = rowField is null ? DecodeElement(property.Value, FieldKind.Text) : DecodeElement(property.Value, rowField);
            }

            rows.Add(new TableRowValue(id, values));
        }

        return new TableFieldValue(rows, rowModel);
    }

    /// <summary>Names the JSON encoding of a value kind, for tests and diagnostics.</summary>
    public static string EncodingOf(FieldValue value) => value switch
    {
        NullFieldValue => "null",
        BoolFieldValue => "boolean",
        IntFieldValue => "integer",
        DecimalFieldValue => "string",
        TextFieldValue or SelectFieldValue => "string",
        DateFieldValue or TimeFieldValue or DateTimeFieldValue => "string",
        LinkFieldValue or AttachFieldValue or BytesFieldValue => "string",
        SecretFieldValue => "boolean",
        JsonFieldValue => "embedded-json",
        TableFieldValue => "array",
        MultiSelectFieldValue => "array",
        _ => "unknown",
    };

    /// <summary>
    /// Writes a scalar value in the contract's JSON encoding into
    /// <paramref name="writer"/> under <paramref name="name"/>.
    /// </summary>
    public static void WriteScalar(Utf8JsonWriter writer, string name, FieldValue value)
    {
        writer.WritePropertyName(name);
        WriteValue(writer, value);
    }

    /// <summary>Writes the value's JSON encoding (the value itself, no property name).</summary>
    public static void WriteValue(Utf8JsonWriter writer, FieldValue value)
    {
        switch (value)
        {
            case NullFieldValue:
                writer.WriteNullValue();
                break;
            case BoolFieldValue b:
                writer.WriteBooleanValue(b.Value);
                break;
            case IntFieldValue i:
                writer.WriteNumberValue(i.Value);
                break;
            case DecimalFieldValue d:
                writer.WriteStringValue(d.CanonicalText);
                break;
            case TextFieldValue t:
                writer.WriteStringValue(t.Value);
                break;
            case SelectFieldValue s:
                writer.WriteStringValue(s.Value);
                break;
            case MultiSelectFieldValue selected:
                writer.WriteStartArray();
                foreach (var option in selected.Values) { writer.WriteStringValue(option); }
                writer.WriteEndArray();
                break;
            case DateFieldValue date:
                writer.WriteStringValue(TemporalFormat.WriteDate(date.Value));
                break;
            case TimeFieldValue time:
                writer.WriteStringValue(TemporalFormat.WriteTime(time.Value));
                break;
            case DateTimeFieldValue dt:
                writer.WriteStringValue(TemporalFormat.WriteDateTime(dt.Value));
                break;
            case LinkFieldValue link:
                WriteNullableText(writer, link.Id);
                break;
            case AttachFieldValue attach:
                WriteNullableText(writer, attach.FileId);
                break;
            case SecretFieldValue secret:
                writer.WriteBooleanValue(secret.IsSet);
                break;
            case BytesFieldValue bytes:
                writer.WriteStringValue(bytes.Base64);
                break;
            case JsonFieldValue json:
                using (var document = JsonDocument.Parse(json.JsonText, DocumentOptions))
                {
                    document.RootElement.WriteTo(writer);
                }

                break;
            case TableFieldValue:
                // A table is written by the draft's table patch writer, never here.
                writer.WriteNullValue();
                break;
            default:
                writer.WriteNullValue();
                break;
        }
    }

    private static void WriteNullableText(Utf8JsonWriter writer, string? text)
    {
        if (text is null)
        {
            writer.WriteNullValue();
        }
        else
        {
            writer.WriteStringValue(text);
        }
    }
}