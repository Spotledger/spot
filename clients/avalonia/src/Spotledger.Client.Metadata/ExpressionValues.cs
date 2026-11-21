using System.Globalization;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Metadata;

/// <summary>
/// Converts a draft's <see cref="FieldValue"/> to the shared expression language's
/// <see cref="ExpressionValue"/> and back (ADR-28: one language, the same results on the
/// client and the server). Exactness is preserved both ways: a decimal becomes the
/// language's mantissa-and-scale value through its canonical digits, never through a
/// <see cref="double"/>, and comes back through the same digits. A value the language
/// cannot express (a child table, a time of day, a decimal wider than the language's 28
/// digits) reads as null or as its canonical text, never as an approximation.
/// </summary>
internal static class ExpressionValues
{
    /// <summary>The most significant digits the language's decimal carries.</summary>
    private const int LanguageDigits = 28;

    /// <summary>
    /// Reads a field value as a language value. <c>null</c> when the value has no
    /// representation the language can compare (a child table); a decimal wider than the
    /// language's 28 digits also reads as the single null, because the language cannot
    /// hold it and a rounded copy would be a lie.
    /// </summary>
    public static ExpressionValue ToExpression(FieldValue? value) => value switch
    {
        null or NullFieldValue => ExpressionValue.Null,
        BoolFieldValue b => new BoolValue(b.Value),
        IntFieldValue i => new IntValue(i.Value),
        DecimalFieldValue d => DecimalToExpression(d.CanonicalText),
        TextFieldValue t => new TextValue(t.Value),
        SelectFieldValue s => new TextValue(s.Value),
        MultiSelectFieldValue m => new ListValue(TextList(m.Values)),
        DateFieldValue date => Date(date.Value),
        // The language has no time type: a time of day is its contract text.
        TimeFieldValue time => new TextValue(TemporalFormat.WriteTime(time.Value)),
        DateTimeFieldValue moment => Moment(moment.Value),
        LinkFieldValue link => link.Id is null ? ExpressionValue.Null : new TextValue(link.Id),
        AttachFieldValue attach => attach.FileId is null ? ExpressionValue.Null : new TextValue(attach.FileId),
        SecretFieldValue secret => new BoolValue(secret.IsSet),
        BytesFieldValue bytes => new TextValue(bytes.Base64),
        JsonFieldValue json => new TextValue(json.JsonText),
        // A child table is rows of values, not a language value.
        _ => ExpressionValue.Null,
    };

    private static List<ExpressionValue> TextList(IReadOnlyList<string> values)
    {
        var items = new ExpressionValue[values.Count];
        for (var i = 0; i < values.Count; i++)
        {
            items[i] = new TextValue(values[i]);
        }

        return new List<ExpressionValue>(items);
    }

    /// <summary>An exact decimal as the language's mantissa and scale.</summary>
    private static ExpressionValue DecimalToExpression(string canonical)
    {
        if (!DecimalFormat.TryParseExact(canonical, out var digits, out var fraction, out var negative))
        {
            return ExpressionValue.Null;
        }

        var significant = (digits + fraction).TrimStart('0').Length;
        if (significant > LanguageDigits || fraction.Length > LanguageDigits)
        {
            // Wider than the language's decimal: it cannot be read exactly, so it is
            // not read at all. The field's own validation still sees the real value.
            return ExpressionValue.Null;
        }

        var mantissaDigits = (digits == "0" ? string.Empty : digits) + fraction;
        if (mantissaDigits.Length == 0)
        {
            return new DecimalValue(Int128.Zero, 0);
        }

        if (!Int128.TryParse(mantissaDigits, NumberStyles.None, CultureInfo.InvariantCulture, out var mantissa))
        {
            return ExpressionValue.Null;
        }

        return new DecimalValue(negative ? -mantissa : mantissa, fraction.Length);
    }

    private static ExpressionValue Date(DateOnly value) =>
        ExpressionDate.FromYearMonthDay(value.Year, value.Month, value.Day) is { } date
            ? new DateValue(date)
            : ExpressionValue.Null;

    private static ExpressionValue Moment(DateTimeOffset value) =>
        ExpressionDateTime.FromMicros(Micros(value)) is { } moment
            ? new DateTimeValue(moment)
            : ExpressionValue.Null;

    /// <summary>Microseconds from the epoch (the language's datetime resolution).</summary>
    private static long Micros(DateTimeOffset value) =>
        (value.ToUniversalTime().UtcTicks - DateTimeOffset.UnixEpoch.UtcTicks) / 10;

    /// <summary>
    /// Writes a language value as the field's kind of value (a <c>default_expr</c>'s
    /// result). Answers false when the value is not a value that field can hold, so the
    /// caller keeps the static default and reports a diagnostic instead of storing a
    /// value the field's own validation would reject.
    /// </summary>
    public static bool TryToFieldValue(ExpressionValue value, FieldModel field, out FieldValue result)
    {
        result = FieldValue.Null;
        ArgumentNullException.ThrowIfNull(field);

        if (value is NullValue)
        {
            return true;
        }

        switch (value)
        {
            case BoolValue boolean:
                if (field.Kind != FieldKind.Bool)
                {
                    return false;
                }

                result = new BoolFieldValue(boolean.Value);
                return true;

            case IntValue integer when field.Kind == FieldKind.IntegerNumber:
                result = new IntFieldValue(integer.Value);
                return true;

            case IntValue integer when field.Kind == FieldKind.DecimalNumber:
                result = new DecimalFieldValue(new decimal(integer.Value));
                return true;

            case DecimalValue exact when field.Kind == FieldKind.DecimalNumber:
                result = ValueCodec.DecodeDecimalString(exact.ToString());
                return true;

            case TextValue text:
                return TryTextValue(text.Value, field, out result);

            case DateValue date when field.Kind == FieldKind.Date:
                {
                    var (year, month, day) = date.Value.YearMonthDay;
                    result = new DateFieldValue(new DateOnly(year, month, day));
                    return true;
                }

            case DateValue date:
                return TryTextValue(date.ToString(), field, out result);

            case DateTimeValue moment when field.Kind == FieldKind.Datetime:
                {
                    if (!TemporalFormat.TryParseDateTime(moment.Value.ToString(), out var parsed))
                    {
                        return false;
                    }

                    result = new DateTimeFieldValue(parsed);
                    return true;
                }

            case ListValue list when field.Multiple && field.Kind == FieldKind.Select:
                {
                    var chosen = new List<string>(list.Values.Count);
                    foreach (var item in list.Values)
                    {
                        if (item is not TextValue itemText || !field.Options.Contains(itemText.Value, StringComparer.Ordinal))
                        {
                            return false;
                        }

                        if (!chosen.Contains(itemText.Value, StringComparer.Ordinal))
                        {
                            chosen.Add(itemText.Value);
                        }
                    }

                    result = new MultiSelectFieldValue(Array.AsReadOnly(
                        field.Options.Where(option => chosen.Contains(option, StringComparer.Ordinal)).ToArray()));
                    return true;
                }

            default:
                return false;
        }
    }

    private static bool TryTextValue(string text, FieldModel field, out FieldValue result)
    {
        result = FieldValue.Null;
        switch (field.Kind)
        {
            case FieldKind.Text:
            case FieldKind.LongText:
            case FieldKind.Json:
                result = new TextFieldValue(text);
                return true;
            case FieldKind.Select when field.Multiple:
                return false;
            case FieldKind.Select:
                if (!field.Options.Contains(text, StringComparer.Ordinal))
                {
                    return false;
                }

                result = new SelectFieldValue(text);
                return true;
            case FieldKind.Link:
            case FieldKind.DynamicLink:
                result = new LinkFieldValue(text, null);
                return true;
            case FieldKind.Attach:
                result = new AttachFieldValue(text);
                return true;
            case FieldKind.Bytes:
                result = new BytesFieldValue(text);
                return true;
            case FieldKind.Date:
                if (!TemporalFormat.TryParseDate(text, out var date))
                {
                    return false;
                }

                result = new DateFieldValue(date);
                return true;
            case FieldKind.Datetime:
                if (!TemporalFormat.TryParseDateTime(text, out var moment))
                {
                    return false;
                }

                result = new DateTimeFieldValue(moment);
                return true;
            case FieldKind.Time:
                if (!TemporalFormat.TryParseTime(text, out var time))
                {
                    return false;
                }

                result = new TimeFieldValue(time);
                return true;
            default:
                return false;
        }
    }
}
