using System.Globalization;

namespace Spotledger.Client.Metadata;

/// <summary>
/// Culture-invariant parse/format of the kinds' typed text entry (the brief's
/// <c>FieldModel.TryParse</c> support): the contract's decimal encoding
/// (<c>^-?[0-9]+(\.[0-9]+)?$</c>), its date and time forms, and the thousand-separator
/// option that is accepted only when the <see cref="NumberParsingSettings" /> allow it.
/// </summary>
public static class DecimalFormat
{
    /// <summary>
    /// Parses the contract's decimal encoding: optional minus, digits, optional
    /// fraction of digits. Never accepts an exponent, grouping, or trailing sign.
    /// Answers the digit groups and whether a minus was present.
    /// </summary>
    public static bool TryParseExact(
        string text,
        out string digits,
        out string fraction,
        out bool negative)
    {
        digits = string.Empty;
        fraction = string.Empty;
        negative = false;

        if (string.IsNullOrEmpty(text))
        {
            return false;
        }

        var index = 0;
        if (text[0] == '-')
        {
            negative = true;
            index = 1;
            if (text.Length == 1)
            {
                return false;
            }
        }

        var wholeStart = index;
        while (index < text.Length && text[index] >= '0' && text[index] <= '9')
        {
            index++;
        }

        var whole = text[wholeStart..index];
        if (whole.Length == 0)
        {
            return false;
        }

        if (index == text.Length)
        {
            digits = StripLeadingZeros(whole);
            return true;
        }

        if (text[index] != '.')
        {
            return false;
        }

        index++;
        var fractionStart = index;
        while (index < text.Length && text[index] >= '0' && text[index] <= '9')
        {
            index++;
        }

        var frac = text[fractionStart..index];
        if (frac.Length == 0 || index != text.Length)
        {
            return false;
        }

        digits = StripLeadingZeros(whole);
        fraction = frac;
        return true;
    }

    /// <summary>
    /// Parses a decimal the user typed: the contract's plain form always works;
    /// thousands separators (<c>,</c> and a space) only when
    /// <paramref name="allowSeparators"/> is set, and then only in the whole part.
    /// </summary>
    public static bool TryParseUser(string text, bool allowSeparators, out string canonical)
    {
        canonical = string.Empty;
        if (string.IsNullOrWhiteSpace(text))
        {
            return false;
        }

        var candidate = allowSeparators ? RemoveSeparators(text) : text;
        if (candidate is null)
        {
            return false;
        }

        if (!TryParseExact(candidate, out var digits, out var fraction, out var negative))
        {
            return false;
        }

        canonical = (negative ? "-" : string.Empty)
            + (digits.Length == 0 ? "0" : digits)
            + (fraction.Length > 0 ? "." + fraction : string.Empty);
        return digits.Length > 0 || fraction.Length > 0;
    }

    private static string? RemoveSeparators(string text)
    {
        // A separator must be between digits: "1,234.50" ok, "1,23" with a lone
        // separator is still digits around it, "1,,2" is not, ",123" is not.
        var sb = new System.Text.StringBuilder(text.Length);
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (c == ',' || c == ' ')
            {
                var beforeDigit = i > 0 && IsDigit(text[i - 1]);
                var afterDigitOrDot = i + 1 < text.Length && (IsDigit(text[i + 1]) || text[i + 1] == '.');
                if (!beforeDigit || !afterDigitOrDot)
                {
                    return null;
                }

                continue;
            }

            sb.Append(c);
        }

        return sb.ToString();
    }

    private static bool IsDigit(char c) => c >= '0' && c <= '9';

    private static string StripLeadingZeros(string digits)
    {
        var trimmed = digits.TrimStart('0');
        return trimmed.Length == 0 ? "0" : trimmed;
    }
}

/// <summary>
/// Culture-invariant parsing rules for the typed kinds: dates as
/// <c>YYYY-MM-DD</c>, times as <c>HH:MM:SS[.ffffff]</c>, datetimes as RFC 3339.
/// </summary>
public static class TemporalFormat
{
    /// <summary>Parses the contract's <c>YYYY-MM-DD</c> date.</summary>
    public static bool TryParseDate(string text, out DateOnly value)
    {
        value = default;
        if (text.Length != 10 || text[4] != '-' || text[7] != '-')
        {
            return false;
        }

        return DateOnly.TryParseExact(text, "yyyy-MM-dd", CultureInfo.InvariantCulture, DateTimeStyles.None, out value);
    }

    /// <summary>Parses the contract's <c>HH:MM:SS[.ffffff]</c> time.</summary>
    public static bool TryParseTime(string text, out TimeOnly value)
    {
        value = default;
        if (text.Length < 8 || text[2] != ':' || text[5] != ':')
        {
            return false;
        }

        return TimeOnly.TryParseExact(text, TimeFormats, CultureInfo.InvariantCulture, DateTimeStyles.None, out value);
    }

    private static readonly string[] TimeFormats = { "HH':'mm':'ss", "HH':'mm':'ss'.'ffffff" };

    /// <summary>Parses the contract's RFC 3339 datetime into a
    /// <see cref="DateTimeOffset"/> with its offset (requests may carry any offset and
    /// the server converts to UTC).</summary>
    public static bool TryParseDateTime(string text, out DateTimeOffset value)
    {
        value = default;
        if (text.Length < 20)
        {
            return false;
        }

        return DateTimeOffset.TryParseExact(
            text,
            DateTimeFormats,
            CultureInfo.InvariantCulture,
            DateTimeStyles.AllowWhiteSpaces | DateTimeStyles.AssumeUniversal,
            out value);
    }

    private static readonly string[] DateTimeFormats =
    {
        "yyyy'-'MM'-'dd'T'HH':'mm':'ss'Z'",
        "yyyy'-'MM'-'dd'T'HH':'mm':'ss'.'FFFFFFF'Z'",
        "yyyy'-'MM'-'dd'T'HH':'mm':'sszzz",
        "yyyy'-'MM'-'dd'T'HH':'mm':'ss'.'FFFFFFFzzz",
    };

    /// <summary>Writes the contract's date encoding.</summary>
    public static string WriteDate(DateOnly value) => value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture);

    /// <summary>Writes the contract's time encoding.</summary>
    public static string WriteTime(TimeOnly value) => value.ToString("HH:mm:ss", CultureInfo.InvariantCulture);

    /// <summary>Writes the contract's datetime encoding: RFC 3339 in UTC.</summary>
    public static string WriteDateTime(DateTimeOffset value) =>
        value.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", CultureInfo.InvariantCulture);
}

/// <summary>
/// Per-call parsing settings a renderer passes down: whether thousands separators are
/// accepted (the brief's "an explicit setting") and, for completeness, whether typed
/// text may carry surrounding whitespace (it may not — the renderer trims first).
/// </summary>
/// <param name="AllowThousandsSeparators">Accept <c>1,234.50</c>-style grouping in
/// decimal fields when true.</param>
public readonly record struct NumberParsingSettings(bool AllowThousandsSeparators)
{
    /// <summary>The default: separators not accepted.</summary>
    public static NumberParsingSettings Strict { get; } = new(false);

    /// <summary>Separators accepted.</summary>
    public static NumberParsingSettings WithSeparators { get; } = new(true);
}