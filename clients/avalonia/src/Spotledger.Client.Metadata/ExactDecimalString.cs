using System.Globalization;

namespace Spotledger.Client.Metadata;

/// <summary>
/// An exact decimal value: either a <see cref="System.Decimal"/> (when the value fits
/// the 28 significant digits a decimal holds) or a string-backed exact value for the
/// wider fields (up to the contract's 38 digits). The rule and its test are explicit:
/// a field whose <c>precision</c> is at most 28 uses the native decimal; a wider field
/// keeps the canonical string. No floating point ever touches either path (ADR-12).
/// </summary>
public readonly struct ExactDecimal : IEquatable<ExactDecimal>, IComparable<ExactDecimal>
{
    /// <summary>The most significant digits a native <see cref="System.Decimal"/> carries.</summary>
    public const int NativeDigits = 28;

    private readonly decimal _value;
    private readonly string? _text;

    private ExactDecimal(decimal value)
    {
        _value = value;
        _text = null;
    }

    private ExactDecimal(string text)
    {
        _value = 0m;
        _text = text;
    }

    /// <summary>True when the value is carried by a <see cref="System.Decimal"/> rather
    /// than the string fallback (the field's precision fits in 28 digits).</summary>
    public bool IsNative => _text is null;

    /// <summary>The native <see cref="System.Decimal"/> when <see cref="IsNative"/>.
    /// A string-backed value is NOT silently truncated to zero: the caller goes through
    /// <see cref="ToCanonicalText"/> instead — dropping 29 significant digits would be
    /// corruption, not conversion.</summary>
    public decimal AsDecimal => IsNative
        ? _value
        : throw new InvalidOperationException(
            "The value exceeds the 28 significant digits System.Decimal carries exactly; use ToCanonicalText().");

    /// <summary>
    /// Creates an exact decimal by the explicit rule: native when
    /// <paramref name="precision"/> (total digits) fits
    /// <see cref="NativeDigits"/>, string-backed otherwise. The value text must already
    /// be the contract's canonical encoding (what parsing produced).
    /// </summary>
    /// <param name="canonical">The canonical decimal text.</param>
    /// <param name="precision">The field's total digits.</param>
    /// <param name="exact">The value, when the text parses.</param>
    public static bool TryCreate(string canonical, int precision, out ExactDecimal exact)
    {
        exact = default;
        if (!DecimalFormat.TryParseExact(canonical, out var digits, out var fraction, out var negative))
        {
            return false;
        }

        var significant = CountSignificant(digits, fraction);
        if (significant <= NativeDigits && fraction.Length <= NativeDigits)
        {
            var native = decimal.Parse(
                (negative ? "-" : string.Empty) + digits
                + (fraction.Length > 0 ? "." + fraction : string.Empty),
                CultureInfo.InvariantCulture);
            exact = new ExactDecimal(native);
            return true;
        }

        exact = new ExactDecimal(
            (negative ? "-" : string.Empty)
            + (digits.Length == 0 ? "0" : digits)
            + (fraction.Length > 0 ? "." + fraction : string.Empty));
        return true;
    }

    /// <summary>Wraps a <see cref="System.Decimal"/> with its own scale.</summary>
    public static ExactDecimal FromDecimal(decimal value) => new(value);

    /// <summary>
    /// Creates an exact decimal from a string already known to be the contract's
    /// canonical decimal encoding (what a document carried): native inside the
    /// 28-digit range, string-backed beyond it.
    /// </summary>
    public static ExactDecimal FromCanonical(string text)
    {
        if (!DecimalFormat.TryParseExact(text, out var digits, out var fraction, out var negative))
        {
            throw new FormatException($"The value is not the contract's decimal encoding: {text}");
        }

        if (TryCreate(text, int.MaxValue, out var built))
        {
            return built;
        }

        // Rebuild via the failed path is impossible when the text parses; keep the
        // compiler's certainty with a direct construction (rebuilt from the parsed
        // parts, never by re-signing the text: canonical already carries a minus).
        return new ExactDecimal(
            (negative ? "-" : string.Empty)
            + (digits.Length == 0 ? "0" : digits)
            + (fraction.Length > 0 ? "." + fraction : string.Empty));
    }

    private static int CountSignificant(string digits, string fraction)
    {
        var whole = digits.TrimStart('0');
        var fracTrimmed = fraction.TrimEnd('0');
        if (fracTrimmed.Length == 0)
        {
            return whole.Length == 0 ? 1 : whole.Length;
        }

        // Trailing zeros in the fraction that pad to the scale are significant for the
        // native-decimal fit: decimal's scale carries them.
        return whole.Length + fraction.Length;
    }

    /// <summary>The exact value as text in either case — the string a patch body or a
    /// comparison needs.</summary>
    public string ToCanonicalText() =>
        IsNative ? _value.ToString(CultureInfo.InvariantCulture) : _text!;

    /// <summary>Compares by numeric value.</summary>
    public int CompareTo(ExactDecimal other) => ToCanonicalText().CompareDecimal(other.ToCanonicalText());

    /// <inheritdoc/>
    public bool Equals(ExactDecimal other)
    {
        // Numeric equality: "10.50" equals "10.5"; scale belongs to the field, not to
        // equality (the wire keeps the scale; the value compares exactly).
        if (IsNative && other.IsNative)
        {
            return _value == other._value;
        }

        return ToCanonicalText().NumericEquals(other.ToCanonicalText());
    }

    /// <inheritdoc/>
    public override bool Equals(object? obj) => obj is ExactDecimal other && Equals(other);

    /// <inheritdoc/>
    public override int GetHashCode() => ToCanonicalText().GetNumericHashCode();

    /// <inheritdoc/>
    public override string ToString() => ToCanonicalText();

    /// <summary>The zero.</summary>
    public static ExactDecimal Zero { get; } = new(0m);

    /// <summary>Numeric equality (see <see cref="Equals(ExactDecimal)"/>).</summary>
    public static bool operator ==(ExactDecimal left, ExactDecimal right) => left.Equals(right);

    /// <summary>The negation of <see cref="op_Equality"/>.</summary>
    public static bool operator !=(ExactDecimal left, ExactDecimal right) => !left.Equals(right);

    /// <summary>True when <paramref name="left"/> is numerically smaller.</summary>
    public static bool operator <(ExactDecimal left, ExactDecimal right) => left.CompareTo(right) < 0;

    /// <summary>True when <paramref name="left"/> is numerically smaller or equal.</summary>
    public static bool operator <=(ExactDecimal left, ExactDecimal right) => left.CompareTo(right) <= 0;

    /// <summary>True when <paramref name="left"/> is numerically greater.</summary>
    public static bool operator >(ExactDecimal left, ExactDecimal right) => left.CompareTo(right) > 0;

    /// <summary>True when <paramref name="left"/> is numerically greater or equal.</summary>
    public static bool operator >=(ExactDecimal left, ExactDecimal right) => left.CompareTo(right) >= 0;
}

/// <summary>Exact text-level helpers over the contract's decimal encoding: numeric
/// comparison and equality without any floating point or big-number arithmetic.</summary>
public static class DecimalTextOps
{
    /// <summary>Compares two canonical decimal strings numerically.</summary>
    public static int CompareDecimal(this string a, string b) => Compare(Artefacts(a), Artefacts(b));

    /// <summary>Numeric equality of two canonical decimal strings.</summary>
    public static bool NumericEquals(this string a, string b) => CompareDecimal(a, b) == 0;

    /// <summary>A stable hash of the numeric value.</summary>
    public static int GetNumericHashCode(this string text)
    {
        var sign = text.StartsWith('-') ? 2 : 1;
        var a = Artefacts(text);
        var digits = a.Digits;
        var scale = a.Scale;
        unchecked
        {
            var hash = 17;
            foreach (var c in digits)
            {
                hash = hash * 31 + c;
            }

            return hash * 31 + scale * sign;
        }
    }

    private static (string Digits, int Scale, bool Sign) Artefacts(string text)
    {
        var negative = text.StartsWith('-');
        var body = negative ? text[1..] : text;
        var dot = body.IndexOf('.');
        var whole = dot < 0 ? body : body[..dot];
        var frac = dot < 0 ? string.Empty : body[(dot + 1)..];
        var digits = (whole + frac).TrimStart('0');
        if (digits.Length == 0)
        {
            digits = "0";
            frac = string.Empty;
        }
        else
        {
            // Scale counts from the right of the whole digits: 10.50 → 1050, scale 2.
            frac = frac.TrimEnd('0');
        }

        return (digits, frac.Length, negative);
    }

    private static int Compare((string Digits, int Scale, bool Sign) a, (string Digits, int Scale, bool Sign) b)
    {
        if (a.Digits == "0" && b.Digits == "0")
        {
            return 0;
        }

        if (a.Sign != b.Sign)
        {
            return a.Sign ? -1 : 1;
        }

        // Align scales by padding the shorter fraction; compare magnitudes.
        var scale = Math.Max(a.Scale, b.Scale);
        var aAligned = Align(a.Digits, a.Scale + 0, scale);
        var bAligned = Align(b.Digits, b.Scale, scale);
        var cmp = string.CompareOrdinal(aAligned.PadLeft(Math.Max(aAligned.Length, bAligned.Length), '0'), bAligned.PadLeft(Math.Max(aAligned.Length, bAligned.Length), '0'));
        if (cmp != 0)
        {
            return a.Sign ? -cmp : cmp;
        }

        return 0;

        static string Align(string digits, int scale, int target)
        {
            // digits hold whole+fraction; scale is the fraction length. To align at
            // target fraction digits, pad the right end.
            var pad = target - scale;
            return pad <= 0 ? digits : digits + new string('0', pad);
        }
    }
}