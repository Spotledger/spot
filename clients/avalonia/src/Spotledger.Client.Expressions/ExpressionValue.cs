using System.Collections.Generic;
using System.Globalization;

namespace Spotledger.Client.Expressions;

/// <summary>
/// A value of the language (specification section 1). Records compare by type and value;
/// decimals compare by mantissa and scale, so equality here is numeric equality only when
/// scales are equal — <see cref="Same" /> is the corpus's comparison (numeric, scale-free);
/// the evaluator's <c>==</c> compares decimals by numeric value whatever the scale.
/// </summary>
public abstract record ExpressionValue
{
    /// <summary>The single null.</summary>
    public static readonly ExpressionValue Null = new NullValue();

    /// <summary>The type's name, for messages.</summary>
    public abstract string TypeName { get; }

    /// <summary>Identity of type, value and (for decimals) scale: the comparison the corpus uses.</summary>
    public bool Same(ExpressionValue other)
    {
        if (ReferenceEquals(this, other))
        {
            return true;
        }
        return (this, other) switch
        {
            (DecimalValue a, DecimalValue b) => a.NumericEquals(b) && a.Scale == b.Scale,
            (ListValue x, ListValue y) =>
                x.Values.Count == y.Values.Count && SameLists(x.Values, y.Values),
            _ => Equals(other),
        };
    }

    private static bool SameLists(IReadOnlyList<ExpressionValue> x, IReadOnlyList<ExpressionValue> y)
    {
        for (int i = 0; i < x.Count; i++)
        {
            if (!x[i].Same(y[i]))
            {
                return false;
            }
        }
        return true;
    }

    /// <summary>The canonical text of the value (the function <c>text</c>); inside a list,
    /// null is written <c>null</c> and text elements are quoted.</summary>
    internal string ToText(bool nested) => this switch
    {
        NullValue => nested ? "null" : string.Empty,
        BoolValue b => b.Value ? "true" : "false",
        IntValue n => IntToString(n.Value),
        DecimalValue d => d.ToString(),
        TextValue t => nested ? ExpressionPrinter.Quote(t.Value) : t.Value,
        DateValue d => d.Value.ToString(),
        DateTimeValue t => t.Value.ToString(),
        ListValue l => "[" + JoinTexts(l.Values) + "]",
        _ => string.Empty,
    };

    private static string JoinTexts(IReadOnlyList<ExpressionValue> items)
    {
        System.Text.StringBuilder sb = new();
        for (int i = 0; i < items.Count; i++)
        {
            if (i > 0)
            {
                sb.Append(", ");
            }
            sb.Append(items[i].ToText(true));
        }
        return sb.ToString();
    }

    /// <summary>An int as decimal digits.</summary>
    internal static string IntToString(long n) => n.ToString(provider: CultureInfo.InvariantCulture);
}

/// <summary>The single null value.</summary>
public sealed record NullValue : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "null";
    /// <summary>The canonical text.</summary>
    public sealed override string ToString() => "null";
}

/// <summary>A boolean.</summary>
public sealed record BoolValue(bool Value) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "bool";
    /// <summary>The canonical text.</summary>
    public override string ToString() => Value ? "true" : "false";
}

/// <summary>A 64-bit signed integer.</summary>
public sealed record IntValue(long Value) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "int";
    /// <summary>The canonical digits.</summary>
    public override string ToString() => ExpressionValue.IntToString(Value);
}

/// <summary>An exact decimal: a signed 128-bit mantissa and its scale (0 to 28).</summary>
public sealed record DecimalValue(Int128 Mantissa, int Scale) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "decimal";

    internal ExpressionDecimal ToDecimal() => new(Mantissa, Scale);

    /// <summary>Builds from mantissa and scale; nothing beyond the language's range.</summary>
    internal static DecimalValue? From(Int128 mantissa, int scale)
    {
        ExpressionDecimal? d = ExpressionDecimal.Make(mantissa, scale);
        return d is null ? null : new DecimalValue(d.Value.Mantissa, d.Value.Scale);
    }

    /// <summary>The canonical text with the scale.</summary>
    public override string ToString() => ToDecimal().ToString();

    internal bool NumericEquals(DecimalValue other) => ToDecimal().NumericEquals(other.ToDecimal());
}

/// <summary>Text (a sequence of Unicode scalar values).</summary>
public sealed record TextValue(string Value) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "text";
    /// <summary>The canonical text, quoted (this form is only nested inside lists; the
    /// function <c>text</c> prints a top-level text bare via <see cref="ExpressionValue" />).</summary>
    public override string ToString() => ExpressionPrinter.Quote(Value);
}

/// <summary>A calendar date, years 0001 to 9999.</summary>
public sealed record DateValue(ExpressionDate Value) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "date";
    /// <summary>The canonical text: YYYY-MM-DD.</summary>
    public override string ToString() => Value.ToString();
}

/// <summary>An instant in UTC with microsecond precision.</summary>
public sealed record DateTimeValue(ExpressionDateTime Value) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "datetime";
    /// <summary>The canonical text.</summary>
    public override string ToString() => Value.ToString();
}

/// <summary>An ordered list of values.</summary>
public sealed record ListValue(IReadOnlyList<ExpressionValue> Values) : ExpressionValue
{
    /// <summary>The type's name.</summary>
    public override string TypeName => "list";
    /// <summary>The canonical text.</summary>
    public override string ToString() => ToText(true);
}