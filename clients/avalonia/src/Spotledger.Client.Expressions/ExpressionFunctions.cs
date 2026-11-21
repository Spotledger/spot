using System;
using System.Collections.Generic;
using System.Globalization;

namespace Spotledger.Client.Expressions;

/// <summary>The function library (specification section 8): mirrors the Rust module,
/// including each function's null rule and the order of type checks.</summary>
internal static class ExpressionFunctions
{
    /// <summary>The number of arguments a function accepts: exactly <c>min</c>, or at least it.
    /// Nothing when there is no such function.</summary>
    internal static (int Min, bool AtLeast)? Arity(string name) => name switch
    {
        "len" or "lower" or "upper" or "trim" or "is_empty" or "abs" or "int" or "decimal"
            or "text" or "date" or "datetime" or "year" or "month" or "day" => (1, false),
        "contains" or "starts_with" or "ends_with" or "round" or "date_add" or "days_between" => (2, false),
        "substr" or "replace" => (3, false),
        "now" => (0, false),
        "coalesce" => (1, true),
        "min" or "max" => (2, true),
        _ => null,
    };

    private static ExpressionEvaluationException TypeError(string function, string what) =>
        new(ExpressionErrorCode.Type, $"`{function}`: {what}");

    /// <summary>The order of two comparable values (numeric, text, date, datetime), or nothing.</summary>
    internal static int? Compare(ExpressionValue a, ExpressionValue b) => (a, b) switch
    {
        (TextValue x, TextValue y) => string.CompareOrdinal(x.Value, y.Value),
        (DateValue x, DateValue y) => x.Value.Days.CompareTo(y.Value.Days),
        (DateTimeValue x, DateTimeValue y) => x.Value.Micros.CompareTo(y.Value.Micros),
        _ when ExpressionEvaluator.AsDecimal(a) is { } da && ExpressionEvaluator.AsDecimal(b) is { } db =>
            da.NumericEquals(db) ? 0 : da.CompareTo(db) < 0 ? -1 : 1,
        _ => null,
    };

    /// <summary>Calls a library function on already evaluated arguments.</summary>
    internal static ExpressionValue Call(string name, ExpressionValue[] args, ExpressionDateTime now)
    {
        ExpressionValue Arg(int i) => i < args.Length ? args[i] : ExpressionValue.Null;
        switch (name)
        {
            case "len":
                return Arg(0) switch
                {
                    NullValue => new IntValue(0),
                    TextValue s => new IntValue(s.Value.Length == 0 ? 0 : CountScalars(s.Value)),
                    ListValue l => new IntValue(l.Values.Count),
                    _ => throw TypeError(name, "expects text or a list"),
                };
            case "lower" or "upper" or "trim":
                return Arg(0) switch
                {
                    NullValue => ExpressionValue.Null,
                    TextValue s => new TextValue(name switch
                    {
                        "lower" => MapCase(s.Value, false),
                        "upper" => MapCase(s.Value, true),
                        _ => TrimSpaces(s.Value),
                    }),
                    _ => throw TypeError(name, "expects text"),
                };
            case "contains":
                return (Arg(0), Arg(1)) switch
                {
                    (NullValue, _) => new BoolValue(false),
                    (TextValue s, TextValue p) => new BoolValue(s.Value.Contains(p.Value, StringComparison.Ordinal)),
                    (TextValue _, NullValue) => new BoolValue(false),
                    (ListValue l, _) => new BoolValue(ListContains(l, Arg(1))),
                    _ => throw TypeError(name, "expects text and text, or a list and a value"),
                };
            case "starts_with" or "ends_with":
                return (Arg(0), Arg(1)) switch
                {
                    (NullValue, _) or (_, NullValue) => new BoolValue(false),
                    (TextValue s, TextValue p) => new BoolValue(name == "starts_with"
                        ? s.Value.StartsWith(p.Value, StringComparison.Ordinal)
                        : s.Value.EndsWith(p.Value, StringComparison.Ordinal)),
                    _ => throw TypeError(name, "expects two texts"),
                };
            case "substr":
                return (Arg(0), Arg(1), Arg(2)) switch
                {
                    (NullValue, _, _) or (_, NullValue, _) or (_, _, NullValue) => ExpressionValue.Null,
                    (TextValue s, IntValue start, IntValue count) => Substring(name, s.Value, start.Value, count.Value),
                    _ => throw TypeError(name, "expects text, an int and an int"),
                };
            case "replace":
                return (Arg(0), Arg(1), Arg(2)) switch
                {
                    (NullValue, _, _) or (_, NullValue, _) or (_, _, NullValue) => ExpressionValue.Null,
                    (TextValue s, TextValue from, TextValue to) => from.Value.Length == 0
                        ? throw TypeError(name, "the text to replace cannot be empty")
                        : new TextValue(s.Value.Replace(from.Value, to.Value, StringComparison.Ordinal)),
                    _ => throw TypeError(name, "expects three texts"),
                };
            case "is_empty":
                return new BoolValue(Arg(0) switch
                {
                    NullValue => true,
                    TextValue s => s.Value.Length == 0,
                    ListValue l => l.Values.Count == 0,
                    _ => false,
                });
            case "coalesce":
                {
                    for (int i = 0; i < args.Length; i++)
                    {
                        if (args[i] is not NullValue)
                        {
                            return args[i];
                        }
                    }
                    return ExpressionValue.Null;
                }
            case "round":
                return Round(name, Arg(0), Arg(1));
            case "abs":
                return Arg(0) switch
                {
                    NullValue => ExpressionValue.Null,
                    IntValue n => new IntValue(AbsInt(n.Value)),
                    DecimalValue d => FromDecimal(ExpressionDecimal.Make(
                        d.Mantissa < 0 ? -d.Mantissa : d.Mantissa,
                        d.Scale)),
                    _ => throw TypeError(name, "expects a number"),
                };
            case "min" or "max":
                return MinMax(name, args);
            case "int":
                return Arg(0) switch
                {
                    NullValue => ExpressionValue.Null,
                    IntValue n => n,
                    DecimalValue d => ToInt(name, d),
                    TextValue s => new IntValue(ParseInt(s.Value)),
                    _ => throw TypeError(name, "expects a number or text"),
                };
            case "decimal":
                return Arg(0) switch
                {
                    NullValue => ExpressionValue.Null,
                    IntValue n => new DecimalValue((Int128)n.Value, 0),
                    DecimalValue d => d,
                    TextValue s => FromDecimal(ParseDecimal(s.Value)),
                    _ => throw TypeError(name, "expects a number or text"),
                };
            case "text":
                return new TextValue(Arg(0).ToText(false));
            case "date":
                return Arg(0) switch
                {
                    NullValue => ExpressionValue.Null,
                    DateValue d => d,
                    DateTimeValue t => new DateValue(t.Value.Date),
                    TextValue s => ParseDate(s.Value),
                    _ => throw TypeError(name, "expects a date, datetime or text"),
                };
            case "datetime":
                return Arg(0) switch
                {
                    NullValue => ExpressionValue.Null,
                    DateTimeValue t => t,
                    DateValue d => new DateTimeValue(ExpressionDateTime.FromDate(d.Value)),
                    TextValue s => ParseDateTime(s.Value),
                    _ => throw TypeError(name, "expects a date, datetime or text"),
                };
            case "now":
                return new DateTimeValue(now);
            case "date_add":
                {
                    ExpressionDate? date = DateOf(name, Arg(0));
                    ExpressionValue n = Arg(1);
                    if (date is null || n is NullValue)
                    {
                        return ExpressionValue.Null;
                    }
                    if (n is not IntValue days)
                    {
                        throw TypeError(name, "expects a date and an int");
                    }
                    long target = (long)date.Value.Days + days.Value;
                    ExpressionDate? moved = target >= int.MinValue && target <= int.MaxValue
                        ? ExpressionDate.FromDays((int)target)
                        : null;
                    return moved is null
                        ? throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the date is out of range")
                        : new DateValue(moved.Value);
                }
            case "days_between":
                {
                    ExpressionDate? a = DateOf(name, Arg(0));
                    ExpressionDate? b = DateOf(name, Arg(1));
                    if (a is null || b is null)
                    {
                        return ExpressionValue.Null;
                    }
                    return new IntValue((long)b.Value.Days - a.Value.Days);
                }
            case "year" or "month" or "day":
                {
                    ExpressionDate? date = DateOf(name, Arg(0));
                    if (date is null)
                    {
                        return ExpressionValue.Null;
                    }
                    (int y, int m, int day) = date.Value.YearMonthDay;
                    return new IntValue(name switch
                    {
                        "year" => y,
                        "month" => m,
                        _ => day,
                    });
                }
            default:
                throw new ExpressionEvaluationException(
                    ExpressionErrorCode.UnknownFunction,
                    $"`{name}` is not a function");
        }
    }

    private static bool ListContains(ListValue l, ExpressionValue y)
    {
        for (int i = 0; i < l.Values.Count; i++)
        {
            if (ExpressionEvaluator.ValuesEqual(l.Values[i], y))
            {
                return true;
            }
        }
        return false;
    }

    private static IntValue ToInt(string name, DecimalValue d)
    {
        Int128 truncated = ExpressionDecimal.Trunc(d.ToDecimal());
        try
        {
            return new IntValue(checked((long)truncated));
        }
        catch (OverflowException)
        {
            throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the integer is out of range");
        }
    }

    private static long AbsInt(long n)
    {
        try
        {
            return checked(Math.Abs(n));
        }
        catch (OverflowException)
        {
            throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the integer is out of range");
        }
    }

    private static DecimalValue FromDecimal(ExpressionDecimal? d) =>
        d is null
            ? throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the decimal result is out of range")
            : new DecimalValue(d.Value.Mantissa, d.Value.Scale);

    private static ExpressionValue Round(string name, ExpressionValue x, ExpressionValue n)
    {
        if (x is NullValue || n is NullValue)
        {
            return ExpressionValue.Null;
        }
        ExpressionDecimal? d = ExpressionEvaluator.AsDecimal(x);
        if (d is null)
        {
            throw TypeError(name, "expects a number");
        }
        if (n is not IntValue scaleValue || scaleValue.Value < 0 || scaleValue.Value > 28)
        {
            throw TypeError(name, "the scale is from 0 to 28");
        }
        ExpressionDecimal? rounded = ExpressionDecimal.Round(d.Value, (int)scaleValue.Value);
        if (rounded is null)
        {
            throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the decimal result is out of range");
        }
        return new DecimalValue(rounded.Value.Mantissa, rounded.Value.Scale);
    }

    private static ExpressionValue MinMax(string name, ExpressionValue[] args)
    {
        for (int i = 0; i < args.Length; i++)
        {
            if (args[i] is NullValue)
            {
                return ExpressionValue.Null;
            }
        }
        ExpressionValue best = args[0];
        for (int i = 1; i < args.Length; i++)
        {
            int? ord = Compare(args[i], best);
            if (ord is null)
            {
                throw TypeError(name, "expects values of one comparable kind");
            }
            bool better = name == "min" ? ord.Value < 0 : ord.Value > 0;
            if (better)
            {
                best = args[i];
            }
        }
        return best;
    }

    /// <summary>Unicode whitespace the language trims: the lexical whitespace and U+00A0.</summary>
    private static string TrimSpaces(string s)
    {
        int start = 0;
        int end = s.Length;
        while (start < end && IsTrimSpace(s[start]))
        {
            start += 1;
        }
        while (end > start && IsTrimSpace(s[end - 1]))
        {
            end -= 1;
        }
        return s[start..end];
    }

    private static bool IsTrimSpace(char c) => c is ' ' or '\t' or '\n' or '\r' or '\u00a0';

    /// <summary>Simple (one scalar to one scalar) case mapping; characters with a longer
    /// mapping (like ß) stay unchanged, as the Rust implementation does.</summary>
    private static string MapCase(string s, bool upper)
    {
        System.Text.StringBuilder sb = new(s.Length);
        for (int i = 0; i < s.Length; i++)
        {
            char c = s[i];
            if (char.IsSurrogatePair(s, i))
            {
                string scalar = s.Substring(i, 2);
                sb.Append(MapScalar(scalar, upper));
                i += 1;
                continue;
            }
            sb.Append(MapScalar(s.Substring(i, 1), upper));
        }
        return sb.ToString();
    }

    private static string MapScalar(string scalar, bool upper)
    {
        string mapped = upper
            ? scalar.ToUpperInvariant()
            : scalar.ToLowerInvariant();
        // Count Unicode scalar values, not UTF-16 units.
        if (CountScalars(mapped) == 1)
        {
            return mapped;
        }
        return scalar;
    }

    /// <summary>The number of Unicode scalar values in a string.</summary>
    internal static int CountScalars(string s)
    {
        int count = 0;
        for (int i = 0; i < s.Length; i++)
        {
            if (char.IsSurrogate(s[i]) && i + 1 < s.Length && char.IsLowSurrogate(s[i + 1]))
            {
                i += 1;
            }
            count += 1;
        }
        return count;
    }

    private static TextValue Substring(string name, string s, long start, long count)
    {
        if (start < 0 || count < 0)
        {
            throw TypeError(name, "start and count cannot be negative");
        }
        // Clamp to the text: skip and take clip (start beyond the end gives the empty text).
        long skip = Math.Min(start, CountScalars(s));
        int utf16Start = Utf16Index(s, skip);
        long remaining = CountScalars(s.Length > skip || skip <= CountScalars(s) ? s[utf16Start..] : string.Empty);
        long take = Math.Min(count, remaining);
        int utf16End = Utf16Index(s, skip + take);
        return new TextValue(s[utf16Start..utf16End]);
    }

    private static int Utf16Index(string s, long scalarIndex)
    {
        int index = 0;
        long seen = 0;
        while (index < s.Length && seen < scalarIndex)
        {
            index += char.IsSurrogatePair(s, index) ? 2 : 1;
            seen += 1;
        }
        return index;
    }

    /// <summary>Parses an optionally signed integer (the function <c>int</c> on text).</summary>
    internal static long ParseInt(string text)
    {
        string body = text.StartsWith('+') ? text[1..] : text;
        string digits = body.StartsWith('-') ? body[1..] : body;
        if (digits.Length == 0 || !IsAsciiDigits(digits))
        {
            throw new ExpressionEvaluationException(ExpressionErrorCode.BadNumber, "the text is not an integer");
        }
        if (!long.TryParse(body, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long n))
        {
            throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the integer is out of range");
        }
        return n;
    }

    /// <summary>Parses an optionally signed decimal literal: <c>[+-]? digits ( '.' digits )?</c>
    /// (the function <c>decimal</c> on text).</summary>
    internal static ExpressionDecimal ParseDecimal(string text)
    {
        static ExpressionEvaluationException Bad() =>
            new(ExpressionErrorCode.BadNumber, "the text is not a decimal number");
        string body = text;
        bool negative = false;
        if (body.StartsWith('-'))
        {
            negative = true;
            body = body[1..];
        }
        else if (body.StartsWith('+'))
        {
            body = body[1..];
        }
        string intPart;
        string fracPart;
        int dot = body.IndexOf('.');
        if (dot >= 0)
        {
            intPart = body[..dot];
            fracPart = body[(dot + 1)..];
            if (fracPart.Length == 0)
            {
                throw Bad();
            }
        }
        else
        {
            intPart = body;
            fracPart = string.Empty;
        }
        if (intPart.Length == 0 || !IsAsciiDigits(intPart) || !IsAsciiDigits(fracPart))
        {
            throw Bad();
        }
        ExpressionDecimal? d = ExpressionDecimal.MakeFromDigits(intPart.AsSpan(), fracPart.AsSpan());
        if (d is null)
        {
            throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the decimal result is out of range");
        }
        Int128 mantissa = negative ? -d.Value.Mantissa : d.Value.Mantissa;
        return new ExpressionDecimal(mantissa, d.Value.Scale);
    }

    private static bool IsAsciiDigits(string s)
    {
        foreach (char c in s)
        {
            if (!char.IsAsciiDigit(c))
            {
                return false;
            }
        }
        return true;
    }

    private static ExpressionDate? DateOf(string function, ExpressionValue v) => v switch
    {
        NullValue => null,
        DateValue d => d.Value,
        DateTimeValue t => t.Value.Date,
        _ => throw TypeError(function, "expects a date"),
    };

    private static DateValue ParseDate(string text) =>
        ExpressionDate.Parse(text) is { } d
            ? new DateValue(d)
            : throw new ExpressionEvaluationException(ExpressionErrorCode.BadDate, "the text is not a date");

    private static DateTimeValue ParseDateTime(string text) =>
        ExpressionDateTime.Parse(text) is { } t
            ? new DateTimeValue(t)
            : throw new ExpressionEvaluationException(ExpressionErrorCode.BadDate, "the text is not a datetime");
}