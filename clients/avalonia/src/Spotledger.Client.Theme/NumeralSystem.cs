using System.Globalization;
using System.Text;

namespace Spotledger.Client.Theme;

/// <summary>
/// Which digit set a value is <em>displayed</em> with. Ledgers and bank documents in Pakistan
/// are written with Latin digits, so <see cref="Latin"/> is the default; the setting changes
/// display only and never the stored or typed value (ADR-12 keeps money a
/// <see cref="decimal"/>, and parsing is always done on Latin digits).
/// </summary>
public enum NumeralSystem
{
    /// <summary>0-9. The default.</summary>
    Latin,

    /// <summary>Arabic-Indic digits, U+0660-U+0669 (the Arabic-language convention).</summary>
    ArabicIndic,

    /// <summary>Extended Arabic-Indic digits, U+06F0-U+06F9 (the Persian/Urdu convention).</summary>
    ExtendedArabicIndic,
}

/// <summary>
/// Digit-set conversion and input normalisation. Everything here is a pure string or
/// <see cref="decimal"/> operation: no floating point, no culture lookup (the digit set is a
/// document setting, not the OS culture), and no wall clock.
///
/// Typed Eastern digits are accepted by normalising them to Latin before parsing, so a value
/// typed as <c>١٢٣٫٤٥</c> or <c>۱۲۳٫۴۵</c> commits as the same <see cref="decimal"/>. The
/// normaliser is the piece a numeric input calls on its edit text; wiring it into
/// <c>SpotDecimalInput</c> belongs to the control's owner (see
/// <c>clients/avalonia/docs/urdu-rtl.md</c>).
/// </summary>
public static class NumeralText
{
    /// <summary>U+066B, the Arabic decimal separator the Eastern digit sets use.</summary>
    public const char ArabicDecimalSeparator = '\u066B';

    /// <summary>U+066C, the Arabic thousands separator.</summary>
    public const char ArabicThousandsSeparator = '\u066C';

    private const int ArabicIndicZero = 0x0660;
    private const int ExtendedArabicIndicZero = 0x06F0;

    /// <summary>The zero digit of a digit set.</summary>
    public static char ZeroDigit(NumeralSystem system) => system switch
    {
        NumeralSystem.Latin => '0',
        NumeralSystem.ArabicIndic => (char)ArabicIndicZero,
        NumeralSystem.ExtendedArabicIndic => (char)ExtendedArabicIndicZero,
        _ => throw new ArgumentOutOfRangeException(nameof(system), system, "Unknown numeral system."),
    };

    /// <summary>Maps the ASCII digits in <paramref name="latinText"/> onto a digit set.</summary>
    public static string ToSystemDigits(string? latinText, NumeralSystem system)
    {
        if (string.IsNullOrEmpty(latinText) || system == NumeralSystem.Latin)
        {
            return latinText ?? string.Empty;
        }

        var zero = ZeroDigit(system);
        return string.Create(latinText.Length, (latinText, zero), static (span, state) =>
        {
            var (source, first) = state;
            for (var index = 0; index < source.Length; index++)
            {
                var character = source[index];
                span[index] = character is >= '0' and <= '9' ? (char)(first + (character - '0')) : character;
            }
        });
    }

    /// <summary>
    /// Maps both Eastern digit sets back to ASCII and the Arabic separators to their Latin
    /// forms (U+066B to '.', U+066C and U+060C to ','). Used before any parse.
    /// </summary>
    public static string ToLatinDigits(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        var builder = new StringBuilder(text.Length);
        foreach (var character in text)
        {
            builder.Append(character switch
            {
                >= (char)ArabicIndicZero and <= (char)(ArabicIndicZero + 9) => (char)('0' + (character - ArabicIndicZero)),
                >= (char)ExtendedArabicIndicZero and <= (char)(ExtendedArabicIndicZero + 9) => (char)('0' + (character - ExtendedArabicIndicZero)),
                ArabicDecimalSeparator => '.',
                ArabicThousandsSeparator => ',',
                '\u060C' => ',',
                '\u2212' => '-',
                _ => character,
            });
        }

        return builder.ToString();
    }

    /// <summary>True when the text holds Arabic-Indic or Extended Arabic-Indic digits.</summary>
    public static bool ContainsEasternDigits(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return false;
        }

        foreach (var character in text)
        {
            if (character is >= (char)ArabicIndicZero and <= (char)(ArabicIndicZero + 9)
                or >= (char)ExtendedArabicIndicZero and <= (char)(ExtendedArabicIndicZero + 9))
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>
    /// Turns typed numeric text into the canonical Latin edit form a numeric input commits, or
    /// refuses it with a reason. Trimming, the digit-set mapping and the Arabic decimal separator
    /// are the same in both directions; what changed is that a grouping separator is only
    /// dropped where the grouping is actually well formed. A separator that could be read as a
    /// decimal separator is <see cref="NumericInputProblem.AmbiguousSeparator"/> and the result
    /// carries the original text, so <c>1,5</c> is refused instead of committing fifteen.
    /// </summary>
    /// <remarks>
    /// Rules, in this order. Every digit set's digits become Latin; U+066B and '.' become the one
    /// decimal point; U+2212 and '-' become minus, and only in the first position. The grouping
    /// candidates are ',' U+066C U+060C NBSP U+202F and the space. A candidate is valid only in
    /// the integer part and only when the groups are either Western (all three digits after a
    /// first group of one to three) or South Asian (a first group of one or two, then groups of
    /// two, and a last group of three), so <c>1,234</c>, <c>12,345,678</c>, <c>12,34,567</c> and
    /// <c>1,00,000</c> are accepted. A comma that could be a decimal comma (<c>1,5</c>,
    /// <c>1,50</c>, <c>12,34</c>, <c>1,5000</c>, and the European <c>1.234,50</c>) is
    /// <see cref="NumericInputProblem.AmbiguousSeparator"/>; any other misplacement is
    /// <see cref="NumericInputProblem.MisplacedGrouping"/>; the decimal point may appear once.
    /// </remarks>
    public static NumericInputResult TryNormalizeNumericInput(string? text)
    {
        var original = text ?? string.Empty;
        var latin = ToLatinDigits(original).Trim();
        if (latin.Length == 0)
        {
            return new NumericInputResult(original, NumericInputProblem.Empty);
        }

        // Digit runs of the integer part, in order. A grouping candidate closes a run, so an
        // empty run is a separator with no digits beside it.
        var groups = new List<int>();
        var digits = new StringBuilder(latin.Length);
        var sign = false;
        var pointSeen = false;
        var fraction = false;
        var fractionDigits = 0;
        var fractionGroupings = 0;
        var digitsAfterFractionGrouping = 0;
        var currentGroup = 0;
        var lastCandidateWasComma = false;

        for (var index = 0; index < latin.Length; index++)
        {
            var character = latin[index];

            if (character is >= '0' and <= '9')
            {
                digits.Append(character);
                if (fraction)
                {
                    fractionDigits++;
                    if (fractionGroupings > 0)
                    {
                        digitsAfterFractionGrouping++;
                    }
                }
                else
                {
                    currentGroup++;
                }

                continue;
            }

            if (character == '-')
            {
                if (index != 0 || sign)
                {
                    return new NumericInputResult(original, NumericInputProblem.UnknownCharacter);
                }

                sign = true;
                continue;
            }

            if (character == '.')
            {
                if (pointSeen)
                {
                    // One decimal point only: a second one is not a number this input accepts.
                    return new NumericInputResult(original, NumericInputProblem.UnknownCharacter);
                }

                pointSeen = true;
                fraction = true;
                groups.Add(currentGroup);
                currentGroup = 0;
                continue;
            }

            if (IsGroupingCandidate(character))
            {
                lastCandidateWasComma = IsCommaFamily(character);
                if (fraction)
                {
                    fractionGroupings++;
                    digitsAfterFractionGrouping = 0;
                    continue;
                }

                groups.Add(currentGroup);
                currentGroup = 0;
                continue;
            }

            return new NumericInputResult(original, NumericInputProblem.UnknownCharacter);
        }

        if (!fraction)
        {
            groups.Add(currentGroup);
        }

        var digitCount = digits.Length;
        if (digitCount == 0)
        {
            // Nothing numeric yet: a sign or a point alone is an empty field being typed in.
            return new NumericInputResult(
                original,
                groups.Count > 1 ? NumericInputProblem.MisplacedGrouping : NumericInputProblem.Empty);
        }

        if (!IsWellFormedGrouping(groups) || fractionGroupings > 0)
        {
            return new NumericInputResult(original, ClassifySeparatorProblem(groups, fractionGroupings, digitsAfterFractionGrouping, lastCandidateWasComma));
        }

        var integerDigits = digitCount - fractionDigits;
        var builder = new StringBuilder(digitCount + 2);
        if (sign)
        {
            builder.Append('-');
        }

        builder.Append(digits.ToString(0, integerDigits));
        if (fractionDigits > 0)
        {
            builder.Append('.');
            builder.Append(digits.ToString(integerDigits, fractionDigits));
        }

        return new NumericInputResult(builder.ToString(), NumericInputProblem.None);
    }

    /// <summary>
    /// The message <b>key</b> a numeric input translates for a refusal — never prose, so the
    /// wording stays in the catalogue. <see cref="NumericInputProblem.None"/> has no message and
    /// yields the empty string.
    /// </summary>
    public static string Explain(NumericInputProblem problem) => problem switch
    {
        NumericInputProblem.None => string.Empty,
        NumericInputProblem.Empty => NumericInputKeys.Empty,
        NumericInputProblem.MisplacedGrouping => NumericInputKeys.MisplacedGrouping,
        NumericInputProblem.AmbiguousSeparator => NumericInputKeys.AmbiguousSeparator,
        NumericInputProblem.UnknownCharacter => NumericInputKeys.UnknownCharacter,
        _ => throw new ArgumentOutOfRangeException(nameof(problem), problem, "Unknown numeric input problem."),
    };

    /// <summary>
    /// Turns typed numeric text into the canonical Latin edit form, or returns it unchanged when
    /// it cannot be read without guessing. The unchanged return is the point: the input keeps
    /// what the user typed, shows <see cref="Explain"/>'s key, and does not commit a number.
    /// </summary>
    public static string NormalizeNumericInput(string? text) => TryNormalizeNumericInput(text).Normalized;

    /// <summary>
    /// Parses numeric text in either digit set into a <see cref="decimal"/>. Never a
    /// <see cref="double"/>: money and quantities stay exact (ADR-12).
    /// </summary>
    public static bool TryParseDecimal(string? text, out decimal value)
    {
        var result = TryNormalizeNumericInput(text);
        if (result.Problem != NumericInputProblem.None)
        {
            value = 0m;
            return false;
        }

        return decimal.TryParse(
            result.Normalized,
            NumberStyles.AllowLeadingSign | NumberStyles.AllowDecimalPoint,
            CultureInfo.InvariantCulture,
            out value);
    }

    /// <summary>True for the characters a keyboard may use where a thousands separator belongs.</summary>
    private static bool IsGroupingCandidate(char character) =>
        character is ',' or '\u066C' or '\u060C' or '\u00A0' or '\u202F' or ' ';

    /// <summary>True for the candidates that also mean "decimal separator" in some keyboards.</summary>
    private static bool IsCommaFamily(char character) => character is ',' or '\u066C' or '\u060C';

    /// <summary>
    /// A single unseparated run of digits is always fine. With separators, the groups must be
    /// Western (a first group of one to three, then threes) or South Asian (a first group of one
    /// or two, then twos, and a last group of three), and no group may be empty.
    /// </summary>
    private static bool IsWellFormedGrouping(List<int> groups)
    {
        foreach (var count in groups)
        {
            if (count <= 0)
            {
                return false;
            }
        }

        if (groups.Count <= 1)
        {
            return groups.Count == 1;
        }

        if (groups[0] > 3)
        {
            return false;
        }

        var western = true;
        for (var index = 1; index < groups.Count; index++)
        {
            if (groups[index] != 3)
            {
                western = false;
                break;
            }
        }

        if (western)
        {
            return true;
        }

        if (groups[0] > 2 || groups[^1] != 3)
        {
            return false;
        }

        for (var index = 1; index < groups.Count - 1; index++)
        {
            if (groups[index] != 2)
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>
    /// Tells "could be a decimal comma" from "just in the wrong place". Only a comma-family
    /// candidate that leaves a final run of digits other than three is ambiguous: <c>1,5</c> is
    /// ten times <c>1.5</c> and a hundred times <c>15</c>'s place, so it is refused; a trailing
    /// separator, an empty group or a space in the wrong place is merely misplaced.
    /// </summary>
    private static NumericInputProblem ClassifySeparatorProblem(
        List<int> groups,
        int fractionGroupings,
        int digitsAfterFractionGrouping,
        bool lastCandidateWasComma)
    {
        if (!lastCandidateWasComma)
        {
            return NumericInputProblem.MisplacedGrouping;
        }

        if (fractionGroupings == 1 && digitsAfterFractionGrouping > 0 && digitsAfterFractionGrouping != 3)
        {
            // 1.234,50 — the comma sits where a European decimal separator would.
            return NumericInputProblem.AmbiguousSeparator;
        }

        if (fractionGroupings == 0 && groups.Count == 2 && groups[0] > 0 && groups[1] > 0 && groups[1] != 3)
        {
            // 1,5 / 1,50 / 12,5 / 12,34 / 1,5000 — one comma, and the run after it is not a group.
            return NumericInputProblem.AmbiguousSeparator;
        }

        return NumericInputProblem.MisplacedGrouping;
    }
}

/// <summary>
/// Money and quantity display for a digit set: which separators to use and how to render a
/// <see cref="decimal"/> with them. Rounding is not done here — the scale is the document's
/// (WP-DOC-02 owns rounding); this only formats.
/// </summary>
public readonly record struct NumeralFormat(string GroupSeparator, string DecimalSeparator, NumeralSystem System)
{
    /// <summary>The separators a digit set is conventionally written with.</summary>
    public static NumeralFormat For(NumeralSystem system) => system switch
    {
        NumeralSystem.Latin => new(",", ".", system),
        NumeralSystem.ArabicIndic => new(
            NumeralText.ArabicThousandsSeparator.ToString(),
            NumeralText.ArabicDecimalSeparator.ToString(),
            system),
        NumeralSystem.ExtendedArabicIndic => new(
            NumeralText.ArabicThousandsSeparator.ToString(),
            NumeralText.ArabicDecimalSeparator.ToString(),
            system),
        _ => throw new ArgumentOutOfRangeException(nameof(system), system, "Unknown numeral system."),
    };

    /// <summary>
    /// Formats a money value: grouped integer part, exactly <paramref name="scale"/> fraction
    /// digits, digits of the configured set. A value that carries more fraction digits than
    /// the scale is shown verbatim rather than silently rounded.
    /// </summary>
    public string Money(decimal value, int scale = 2)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(scale);
        var fractionDigits = Math.Max(value.Scale, scale);
        var text = value.ToString("F" + fractionDigits.ToString(CultureInfo.InvariantCulture), CultureInfo.InvariantCulture);
        var negative = text.StartsWith('-');
        if (negative)
        {
            text = text[1..];
        }

        var point = text.IndexOf('.', StringComparison.Ordinal);
        var integerPart = point < 0 ? text : text[..point];
        var fractionPart = point < 0 ? string.Empty : text[(point + 1)..];
        var grouped = GroupDigits(integerPart, GroupSeparator);
        var builder = new StringBuilder(grouped.Length + fractionPart.Length + 2);
        if (negative)
        {
            builder.Append('-');
        }

        builder.Append(grouped);
        if (fractionPart.Length > 0)
        {
            builder.Append(DecimalSeparator);
            builder.Append(fractionPart);
        }

        return NumeralText.ToSystemDigits(builder.ToString(), System);
    }

    private static string GroupDigits(string digits, string groupSeparator)
    {
        if (groupSeparator.Length == 0 || digits.Length <= 3)
        {
            return digits;
        }

        var result = new StringBuilder(digits.Length + (digits.Length / 3) + 1);
        var first = digits.Length % 3;
        if (first > 0)
        {
            result.Append(digits.AsSpan(0, first));
        }

        for (var index = first; index < digits.Length; index += 3)
        {
            if (result.Length > 0)
            {
                result.Append(groupSeparator);
            }

            result.Append(digits.AsSpan(index, 3));
        }

        return result.ToString();
    }
}

/// <summary>
/// The user setting: which digit set values are displayed with. Default
/// <see cref="NumeralSystem.Latin"/> because ledgers and bank documents use Latin digits; the
/// setting is display-only. The storage form is the enum name, so a settings file round-trips
/// and an unknown or missing value falls back to the default instead of failing a start-up.
/// </summary>
public sealed record NumeralSettings(NumeralSystem System)
{
    /// <summary>Latin digits, the default.</summary>
    public static NumeralSettings Default { get; } = new(NumeralSystem.Latin);

    /// <summary>The format this setting displays with.</summary>
    public NumeralFormat Format => NumeralFormat.For(System);

    /// <summary>Reads a stored setting; null, empty or unknown yields the default.</summary>
    public static NumeralSettings FromStorageValue(string? stored) =>
        Enum.TryParse<NumeralSystem>(stored, ignoreCase: true, out var system)
            && Enum.IsDefined(system)
            ? new(system)
            : Default;

    /// <summary>The value to persist.</summary>
    public string ToStorageValue() => System.ToString();
}
