using System.Globalization;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Browser;

/// <summary>
/// The JSON the browser head exchanges with a calculation WebAssembly module: the typed-value format
/// of the expression conformance corpus (<c>crates/framework/spot-expr/corpus/README.md</c>), so the
/// module and this client read the same shapes. No JSON number ever carries a value: integers and
/// decimals travel as strings, and a decimal keeps its scale (<c>12.50</c> is not <c>12.5</c>).
/// Written and read with <see cref="Utf8JsonWriter"/> and <see cref="JsonDocument"/>: no reflection,
/// safe under trimming and AOT.
/// </summary>
public static class CalcWire
{
    /// <summary>The request as JSON: <c>{"expr", "now", "context": {path: typed}}</c> with sorted context keys.</summary>
    public static string EncodeRequest(CalcRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteString("expr", request.Source);
            writer.WriteString("now", request.Now.ToString());
            writer.WriteStartObject("context");
            foreach (var (path, value) in request.Context.OrderBy(pair => pair.Key, StringComparer.Ordinal))
            {
                writer.WritePropertyName(path);
                WriteValue(writer, value);
            }

            writer.WriteEndObject();
            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>Reads a request produced by <see cref="EncodeRequest"/> (what a module's host side does).</summary>
    public static CalcRequest DecodeRequest(string json)
    {
        ArgumentNullException.ThrowIfNull(json);
        using var document = JsonDocument.Parse(json);
        var root = document.RootElement;
        var now = ExpressionDateTime.Parse(root.GetProperty("now").GetString() ?? string.Empty)
            ?? throw new FormatException("the request has no valid 'now'");
        var context = new Dictionary<string, ExpressionValue>(StringComparer.Ordinal);
        foreach (var property in root.GetProperty("context").EnumerateObject())
        {
            context[property.Name] = ReadValue(property.Value);
        }

        return new CalcRequest(root.GetProperty("expr").GetString() ?? string.Empty, now, context);
    }

    /// <summary>The outcome as JSON: <c>{"ok": typed}</c> or <c>{"error": code[, "offset": n]}</c>.</summary>
    public static string EncodeOutcome(CalcOutcome outcome)
    {
        ArgumentNullException.ThrowIfNull(outcome);
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            if (outcome.IsOk)
            {
                writer.WritePropertyName("ok");
                WriteValue(writer, outcome.Value!);
            }
            else
            {
                writer.WriteString("error", outcome.ErrorCode);
                if (outcome.Offset is { } offset)
                {
                    writer.WriteNumber("offset", offset);
                }
            }

            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>Reads an outcome produced by a module. Malformed JSON is a <see cref="FormatException"/>.</summary>
    public static CalcOutcome DecodeOutcome(string json)
    {
        ArgumentNullException.ThrowIfNull(json);
        try
        {
            using var document = JsonDocument.Parse(json);
            var root = document.RootElement;
            if (root.TryGetProperty("ok", out var ok))
            {
                return CalcOutcome.Ok(ReadValue(ok));
            }

            var code = root.GetProperty("error").GetString() ?? throw new FormatException("the outcome has no error code");
            int? offset = root.TryGetProperty("offset", out var at) ? at.GetInt32() : null;
            return CalcOutcome.Error(code, offset);
        }
        catch (Exception exception) when (exception is JsonException or KeyNotFoundException or InvalidOperationException)
        {
            throw new FormatException("the calculation module answered with malformed JSON", exception);
        }
    }

    private static void WriteValue(Utf8JsonWriter writer, ExpressionValue value)
    {
        writer.WriteStartObject();
        switch (value)
        {
            case NullValue:
                writer.WriteBoolean("null", true);
                break;
            case BoolValue boolean:
                writer.WriteBoolean("bool", boolean.Value);
                break;
            case IntValue integer:
                writer.WriteString("int", integer.Value.ToString(CultureInfo.InvariantCulture));
                break;
            case DecimalValue number:
                writer.WriteString("decimal", number.ToString());
                break;
            case TextValue text:
                writer.WriteString("text", text.Value);
                break;
            case DateValue date:
                writer.WriteString("date", date.Value.ToString());
                break;
            case DateTimeValue instant:
                writer.WriteString("datetime", instant.Value.ToString());
                break;
            case ListValue list:
                writer.WriteStartArray("list");
                foreach (var item in list.Values)
                {
                    WriteValue(writer, item);
                }

                writer.WriteEndArray();
                break;
            default:
                throw new NotSupportedException($"value type {value.TypeName} has no wire form");
        }

        writer.WriteEndObject();
    }

    private static ExpressionValue ReadValue(JsonElement element)
    {
        foreach (var property in element.EnumerateObject())
        {
            switch (property.Name)
            {
                case "null":
                    return ExpressionValue.Null;
                case "bool":
                    return new BoolValue(property.Value.GetBoolean());
                case "int":
                    return new IntValue(long.Parse(property.Value.GetString() ?? "0", CultureInfo.InvariantCulture));
                case "decimal":
                    return ReadDecimal(property.Value.GetString() ?? "0");
                case "text":
                    return new TextValue(property.Value.GetString() ?? string.Empty);
                case "date":
                    return new DateValue(ExpressionDate.Parse(property.Value.GetString() ?? string.Empty)
                        ?? throw new FormatException("bad wire date"));
                case "datetime":
                    return new DateTimeValue(ExpressionDateTime.Parse(property.Value.GetString() ?? string.Empty)
                        ?? throw new FormatException("bad wire datetime"));
                case "list":
                    return new ListValue(property.Value.EnumerateArray().Select(ReadValue).ToArray());
            }
        }

        throw new FormatException("a wire value has no known type key");
    }

    /// <summary>A decimal string ("12.50", optionally signed) as a value: the scale is the digits after the point.</summary>
    private static DecimalValue ReadDecimal(string text)
    {
        var negative = text.StartsWith('-');
        var body = negative || text.StartsWith('+') ? text[1..] : text;
        var dot = body.IndexOf('.', StringComparison.Ordinal);
        var whole = dot >= 0 ? body[..dot] : body;
        var fraction = dot >= 0 ? body[(dot + 1)..] : string.Empty;
        var digits = whole + fraction;
        if (digits.Length == 0 || digits.Length > 38)
        {
            throw new FormatException($"bad wire decimal '{text}'");
        }

        var mantissa = Int128.Parse(negative ? "-" + digits : digits, CultureInfo.InvariantCulture);
        return new DecimalValue(mantissa, fraction.Length);
    }
}
