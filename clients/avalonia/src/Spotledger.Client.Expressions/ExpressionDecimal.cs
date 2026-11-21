using System;
using System.Globalization;

namespace Spotledger.Client.Expressions;

/// <summary>
/// Exact decimal arithmetic on a signed 128-bit mantissa and a scale, mirroring the Rust
/// engine of <c>spot-expr/src/decimal.rs</c>: half-to-even rounding at the scale of the
/// widest operand (at least 6 for division), 28 significant digits at most, and the same
/// overflow boundaries. No floating point anywhere: every failure comes back as null, which
/// the evaluator reports as <c>expr.overflow</c>.
/// </summary>
/// <param name="Mantissa">The signed digits up to 28 of them (an Int128).</param>
/// <param name="Scale">The digits after the point (0 to 28).</param>
internal readonly record struct ExpressionDecimal(Int128 Mantissa, int Scale) : IComparable<ExpressionDecimal>
{
    /// <summary>The most significant digits a decimal may have.</summary>
    internal const int MaxDigits = 28;

    private static readonly Int128 TenPow28 = TenPow(MaxDigits);

    private static Int128 TenPow(int n)
    {
        Int128 value = 1;
        for (int i = 0; i < n; i++)
        {
            value *= 10;
        }
        return value;
    }

    /// <summary>10<paramref name="n" />, or nothing beyond the 128-bit range.</summary>
    internal static Int128? CheckedTenPow(int n)
    {
        if (n < 0)
        {
            return null;
        }
        if (n <= MaxDigits)
        {
            return TenPow(n);
        }
        try
        {
            Int128 value = TenPow(MaxDigits);
            for (int i = MaxDigits; i < n; i++)
            {
                checked
                {
                    value *= 10;
                }
            }
            return value;
        }
        catch (OverflowException)
        {
            return null;
        }
    }

    /// <summary>Builds a decimal from a mantissa and scale; nothing beyond the language's range.</summary>
    internal static ExpressionDecimal? Make(Int128 mantissa, int scale)
    {
        if (scale < 0 || scale > MaxDigits)
        {
            return null;
        }
        return Int128.Abs(mantissa) >= TenPow28 ? null : new ExpressionDecimal(mantissa, scale);
    }

    /// <summary>Builds a decimal from the digits before and after the point (the Rust lexer's
    /// rule): a scale above 28 or more than 28 significant digits fails, as does a digit
    /// string too long for Int128. Leading zeros do not count as significant.</summary>
    internal static ExpressionDecimal? MakeFromDigits(ReadOnlySpan<char> intPart, ReadOnlySpan<char> fracPart)
    {
        if (fracPart.Length > MaxDigits)
        {
            return null;
        }
        Span<char> all = stackalloc char[MaxDigits * 2 + 2];
        if (intPart.Length + fracPart.Length > all.Length)
        {
            return null;
        }
        intPart.CopyTo(all);
        fracPart.CopyTo(all[intPart.Length..]);
        ReadOnlySpan<char> digits = all[..(intPart.Length + fracPart.Length)];
        int significant = digits.TrimStart('0').Length;
        if (significant > MaxDigits)
        {
            return null;
        }
        Int128 mantissa = digits.Length == 0
            ? (Int128)0
            : Int128.Parse(digits, provider: CultureInfo.InvariantCulture);
        return new ExpressionDecimal(mantissa, fracPart.Length);
    }

    /// <summary><c>n / d</c> rounded half to even; nothing on overflow. <paramref name="d" /> is not zero.</summary>
    internal static Int128? DivRound(Int128 n, Int128 d)
    {
        try
        {
            checked
            {
                if (d < 0)
                {
                    n = -n;
                    d = -d;
                }
                Int128 q = n / d;
                Int128 r = n % d;
                if (r == 0)
                {
                    return q;
                }
                Int128 twice = Int128.Abs(r) * 2;
                Int128 magnitude = Int128.Abs(d);
                bool away = twice > magnitude || (twice == magnitude && q % 2 != 0);
                if (!away)
                {
                    return q;
                }
                return n < 0 ? q - 1 : q + 1;
            }
        }
        catch (OverflowException)
        {
            return null;
        }
    }

    /// <summary>Scales a mantissa upward (exact); nothing when that overflows.</summary>
    private static Int128? Up(Int128 m, int from, int to)
    {
        if (to <= from)
        {
            return m;
        }
        Int128? power = CheckedTenPow(to - from);
        if (power is null)
        {
            return null;
        }
        try
        {
            return checked(m * power.Value);
        }
        catch (OverflowException)
        {
            return null;
        }
    }

    private static (Int128? X, Int128? Y) AtScale(ExpressionDecimal a, ExpressionDecimal b, int s) =>
        (Up(a.Mantissa, a.Scale, s), Up(b.Mantissa, b.Scale, s));

    /// <summary>The sum at the wider scale; nothing on overflow.</summary>
    internal static ExpressionDecimal? Add(ExpressionDecimal a, ExpressionDecimal b)
    {
        int s = Math.Max(a.Scale, b.Scale);
        (Int128? x, Int128? y) = AtScale(a, b, s);
        if (x is null || y is null)
        {
            return null;
        }
        try
        {
            return Make(checked(x.Value + y.Value), s);
        }
        catch (OverflowException)
        {
            return null;
        }
    }

    /// <summary>The difference at the wider scale; nothing on overflow.</summary>
    internal static ExpressionDecimal? Sub(ExpressionDecimal a, ExpressionDecimal b)
    {
        int s = Math.Max(a.Scale, b.Scale);
        (Int128? x, Int128? y) = AtScale(a, b, s);
        if (x is null || y is null)
        {
            return null;
        }
        try
        {
            return Make(checked(x.Value - y.Value), s);
        }
        catch (OverflowException)
        {
            return null;
        }
    }

    /// <summary>The product rounded half-even to the wider scale; nothing on overflow.</summary>
    internal static ExpressionDecimal? Mul(ExpressionDecimal a, ExpressionDecimal b)
    {
        int s = Math.Max(a.Scale, b.Scale);
        Int128 product;
        try
        {
            product = checked(a.Mantissa * b.Mantissa);
        }
        catch (OverflowException)
        {
            return null;
        }
        int exactScale = a.Scale + b.Scale;
        int diff = exactScale - s;
        Int128 m;
        if (diff == 0)
        {
            m = product;
        }
        else if (diff > 38)
        {
            m = (Int128)0;
        }
        else
        {
            Int128? power = CheckedTenPow(diff);
            if (power is null)
            {
                return null;
            }
            Int128? rounded = DivRound(product, power.Value);
            if (rounded is null)
            {
                return null;
            }
            return Make(rounded.Value, s);
        }
        return Make(m, s);
    }

    /// <summary>The quotient rounded half-even to scale <c>max(wider, 6)</c>; nothing on overflow.</summary>
    internal static ExpressionDecimal? Div(ExpressionDecimal a, ExpressionDecimal b)
    {
        int t = Math.Max(Math.Max(a.Scale, b.Scale), 6);
        Int128? left = CheckedTenPow(t + b.Scale);
        Int128? right = CheckedTenPow(a.Scale);
        if (left is null || right is null)
        {
            return null;
        }
        Int128 num;
        Int128 den;
        try
        {
            num = checked(a.Mantissa * left.Value);
            den = checked(b.Mantissa * right.Value);
        }
        catch (OverflowException)
        {
            return null;
        }
        Int128? quotient = DivRound(num, den);
        return quotient is null ? null : Make(quotient.Value, t);
    }

    /// <summary>The remainder with the sign of <paramref name="a" />; nothing on overflow.</summary>
    internal static ExpressionDecimal? Rem(ExpressionDecimal a, ExpressionDecimal b)
    {
        int s = Math.Max(a.Scale, b.Scale);
        (Int128? x, Int128? y) = AtScale(a, b, s);
        if (x is null || y is null)
        {
            return null;
        }
        try
        {
            return Make(checked(x.Value % y.Value), s);
        }
        catch (OverflowException)
        {
            return null;
        }
    }

    /// <summary>Rounded half-even to exactly <paramref name="scale" /> places; nothing on overflow.</summary>
    internal static ExpressionDecimal? Round(ExpressionDecimal d, int scale)
    {
        if (d.Scale <= scale)
        {
            Int128? up = Up(d.Mantissa, d.Scale, scale);
            return up is null ? null : Make(up.Value, scale);
        }
        Int128? power = CheckedTenPow(d.Scale - scale);
        if (power is null)
        {
            return null;
        }
        Int128? divided = DivRound(d.Mantissa, power.Value);
        return divided is null ? null : Make(divided.Value, scale);
    }

    /// <summary>The integer part, truncated toward zero (0 when the scale's power overflows,
    /// as the Rust reference does).</summary>
    internal static Int128 Trunc(ExpressionDecimal d)
    {
        Int128? power = CheckedTenPow(d.Scale);
        return power is null ? (Int128)0 : d.Mantissa / power.Value;
    }

    /// <summary>The numeric order of two decimals, whatever their scales.</summary>
    public int CompareTo(ExpressionDecimal other)
    {
        int s = Math.Max(Scale, other.Scale);
        (Int128? x, Int128? y) = AtScale(this, other, s);
        if (x is null || y is null)
        {
            // Unreachable for in-range decimals; compare by sign then magnitude.
            int bySign = (Mantissa > (Int128)0 ? 1 : 0) - (other.Mantissa > (Int128)0 ? 1 : 0);
            return bySign != 0 ? bySign : Int128.Abs(Mantissa).CompareTo(Int128.Abs(other.Mantissa));
        }
        return x.Value.CompareTo(y.Value);
    }

    /// <summary>Whether the numeric value is zero.</summary>
    internal bool IsZero => Mantissa == 0;

    /// <summary>Numeric equality whatever the scales.</summary>
    internal bool NumericEquals(ExpressionDecimal other) => CompareTo(other) == 0;

    /// <summary>The canonical text (Rust <c>print</c>/<c>Display</c> of rust_decimal): the
    /// mantissa's digits, scale of them after the point, no sign on zero.</summary>
    public override string ToString()
    {
        bool negative = Mantissa < 0;
        string digits = Int128.Abs(Mantissa).ToString(provider: CultureInfo.InvariantCulture);
        int scale = Scale;
        if (digits.Length <= scale)
        {
            digits = digits.PadLeft(scale + 1, '0');
        }
        string text = scale == 0 ? digits : digits[..^scale] + "." + digits[^scale..];
        return negative ? "-" + text : text;
    }
}