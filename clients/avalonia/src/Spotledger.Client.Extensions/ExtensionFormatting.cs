using System.Globalization;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>Exact presentation formatting for the data blocks and catalog presentations.
/// Nothing here converts a decimal to <see cref="double" />: a quantity keeps its canonical
/// digits, and a percentage is arithmetic on <see cref="decimal" />.</summary>
public static class ExtensionFormatting
{
    /// <summary>Formats an aggregate for a <c>stat</c> block's declared format.</summary>
    /// <param name="value">The aggregate the query returned.</param>
    /// <param name="format">One of <c>int</c>, <c>decimal</c>, <c>percent</c>, <c>currency</c>.</param>
    /// <param name="currencySymbol">The symbol a <c>currency</c> stat shows, when the shell knows it.</param>
    public static string Stat(ExpressionValue value, string format, string? currencySymbol = null)
    {
        ArgumentNullException.ThrowIfNull(value);
        ArgumentNullException.ThrowIfNull(format);
        return format switch
        {
            "int" => IntegerText(value),
            "decimal" => DecimalText(value),
            "percent" => Percent(value),
            "currency" => Currency(value, currencySymbol),
            _ => DisplayText.For(value)
        };
    }

    /// <summary>The integer an aggregate shows as, or its exact digits.</summary>
    public static string IntegerText(ExpressionValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        return value switch
        {
            IntValue integer => integer.Value.ToString(CultureInfo.InvariantCulture),
            DecimalValue exact => exact.ToString(),
            TextValue text => text.Value,
            NullValue => string.Empty,
            _ => DisplayText.For(value)
        };
    }

    /// <summary>The exact decimal an aggregate shows as (no rounding, never a double).</summary>
    public static string DecimalText(ExpressionValue value) => IntegerText(value);

    /// <summary>The value as a percentage: the exact fraction times one hundred, at most two decimals.</summary>
    public static string Percent(ExpressionValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        return value switch
        {
            IntValue integer => (integer.Value * 100).ToString(CultureInfo.InvariantCulture) + "%",
            DecimalValue exact => (ToDecimal(exact) * 100m).ToString("0.##", CultureInfo.InvariantCulture) + "%",
            TextValue text => text.Value,
            NullValue => string.Empty,
            _ => DisplayText.For(value)
        };
    }

    /// <summary>The value with the currency symbol in front; the digits stay exact.</summary>
    public static string Currency(ExpressionValue value, string? symbol)
    {
        ArgumentNullException.ThrowIfNull(value);
        string digits = IntegerText(value);
        if (digits.Length == 0 || string.IsNullOrEmpty(symbol))
        {
            return digits;
        }
        return digits.StartsWith('-') ? "-" + symbol + digits[1..] : symbol + digits;
    }

    /// <summary>
    /// A progress value as a whole percentage in 0..100. The contract's <c>progress</c>
    /// control carries one value, read as a percentage; a value below zero clamps to zero
    /// and one above a hundred to a hundred, so a document cannot draw outside the bar.
    /// </summary>
    public static int Progress(ExpressionValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        decimal number = value switch
        {
            IntValue integer => integer.Value,
            DecimalValue exact => ToDecimal(exact),
            _ => 0m
        };
        if (number <= 0m)
        {
            return 0;
        }
        if (number >= 100m)
        {
            return 100;
        }
        return (int)decimal.Round(number, 0, MidpointRounding.ToZero);
    }

    /// <summary>The text of a value presentation (number, currency, date).</summary>
    public static string Value(ExpressionValue value, string control, string? currencySymbol)
    {
        ArgumentNullException.ThrowIfNull(value);
        ArgumentNullException.ThrowIfNull(control);
        return control switch
        {
            "currency" => Currency(value, currencySymbol),
            "date" => value is DateValue date ? date.Value.ToString() : DisplayText.For(value),
            _ => IntegerText(value)
        };
    }

    private static decimal ToDecimal(DecimalValue value)
    {
        // The mantissa can exceed decimal's range; fall back to zero for a display-only bar
        // rather than throwing on a hostile value.
        if (value.Mantissa > (Int128)decimal.MaxValue || value.Mantissa < (Int128)decimal.MinValue)
        {
            return 0m;
        }
        decimal mantissa = (decimal)value.Mantissa;
        decimal scale = 1m;
        for (int index = 0; index < value.Scale; index++)
        {
            if (scale > decimal.MaxValue / 10m)
            {
                return 0m;
            }
            scale *= 10m;
        }
        return mantissa / scale;
    }
}
