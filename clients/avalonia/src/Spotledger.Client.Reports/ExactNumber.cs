using System.Globalization;
using System.Numerics;

namespace Spotledger.Client.Reports;

/// <summary>
/// An exact decimal for totals, comparison and the numeric test of the CSV writer: an unscaled
/// integer and a scale, parsed from the plain text the contract uses. No floating-point type is
/// involved anywhere (ADR-12), so a 29-digit value or a long column of cents adds up exactly.
/// </summary>
internal readonly record struct ExactNumber(BigInteger Unscaled, int Scale)
{
    /// <summary>The most digits one number may carry (the contract's decimals stop at 38; this is a guard).</summary>
    private const int MaxDigits = 200;

    /// <summary>Parses <c>[+-]digits[.digits]</c>; anything else (exponent, separators, text) is not a number.</summary>
    public static bool TryParse(string? text, out ExactNumber number)
    {
        number = default;
        if (string.IsNullOrEmpty(text) || text.Length > MaxDigits + 2)
        {
            return false;
        }

        var index = 0;
        var negative = false;
        if (text[0] is '-' or '+')
        {
            negative = text[0] == '-';
            index = 1;
        }

        var digits = new System.Text.StringBuilder(text.Length);
        var scale = 0;
        var seenPoint = false;
        var any = false;
        for (; index < text.Length; index++)
        {
            var c = text[index];
            if (c is >= '0' and <= '9')
            {
                digits.Append(c);
                any = true;
                if (seenPoint)
                {
                    scale++;
                }
            }
            else if (c == '.' && !seenPoint)
            {
                seenPoint = true;
            }
            else
            {
                return false;
            }
        }

        if (!any)
        {
            return false;
        }

        var value = BigInteger.Parse(digits.ToString(), NumberStyles.None, CultureInfo.InvariantCulture);
        number = new ExactNumber(negative ? -value : value, scale);
        return true;
    }

    /// <summary>Compares two numbers by value.</summary>
    public static int Compare(ExactNumber left, ExactNumber right)
    {
        var scale = Math.Max(left.Scale, right.Scale);
        return left.ScaledTo(scale).CompareTo(right.ScaledTo(scale));
    }

    /// <summary>The exact sum; the scale is the larger of the two.</summary>
    public static ExactNumber Add(ExactNumber left, ExactNumber right)
    {
        var scale = Math.Max(left.Scale, right.Scale);
        return new ExactNumber(left.ScaledTo(scale) + right.ScaledTo(scale), scale);
    }

    /// <summary>Plain text with exactly <see cref="Scale"/> fraction digits and a leading minus when negative.</summary>
    public override string ToString()
    {
        var negative = Unscaled.Sign < 0;
        var digits = BigInteger.Abs(Unscaled).ToString(CultureInfo.InvariantCulture);
        if (Scale > 0)
        {
            digits = digits.PadLeft(Scale + 1, '0');
            digits = digits[..^Scale] + "." + digits[^Scale..];
        }

        return negative ? "-" + digits : digits;
    }

    private BigInteger ScaledTo(int scale) => scale == Scale ? Unscaled : Unscaled * BigInteger.Pow(10, scale - Scale);
}
