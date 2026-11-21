using System.Globalization;
using System.Text;
using Avalonia;
using Avalonia.Media;

namespace Spotledger.Client.Theme;

/// <summary>The base direction of a field or run.</summary>
public enum TextDirection
{
    LeftToRight,
    RightToLeft,
}

/// <summary>
/// The kinds of value the client shows. The kind, not the window, decides a field's base
/// direction: an IBAN column stays left-to-right even in an Urdu screen.
/// </summary>
public enum FieldTextKind
{
    /// <summary>A caption or header.</summary>
    Label,

    /// <summary>Free prose: a note, a description, a name.</summary>
    FreeText,

    /// <summary>An item or account code.</summary>
    Code,

    /// <summary>An invoice or voucher number.</summary>
    InvoiceNumber,

    /// <summary>An IBAN or account number.</summary>
    Iban,

    /// <summary>A phone number.</summary>
    Phone,

    /// <summary>An e-mail address.</summary>
    Email,

    /// <summary>A money amount.</summary>
    Money,

    /// <summary>A date in the ISO form ADR-077 displays.</summary>
    Date,

    /// <summary>A postal address.</summary>
    Address,
}

/// <summary>
/// Bidi helpers for the mixed Urdu/Latin/number fields that are the daily case in a Pakistani
/// ERP: an Urdu name with a Latin product code and a number. Two separate jobs live here.
///
/// <para><b>Base direction.</b> <see cref="BaseDirection"/> applies the Unicode bidi
/// algorithm's first-strong-character rule (UAX #9 P2/P3) so a caption or a note follows its
/// own text, and <see cref="FieldDirection"/> overrides that for value kinds whose direction
/// is fixed by their content kind (codes, IBANs, phone numbers, dates, money, e-mail).</para>
///
/// <para><b>Isolates.</b> A code-like value embedded in Urdu text is wrapped in Unicode
/// isolates (U+2066 LRI … U+2069 PDI) so the surrounding right-to-left run cannot reorder it.
/// Isolates are display-only: <see cref="ForDisplay"/> returns a string to render, never a
/// value to store, and <see cref="RemoveIsolates"/> restores the exact original so copied
/// text and round-tripped values are unchanged.</para>
/// </summary>
public static class BidiText
{
    /// <summary>U+2066, left-to-right isolate.</summary>
    public const char LeftToRightIsolate = '\u2066';

    /// <summary>U+2067, right-to-left isolate.</summary>
    public const char RightToLeftIsolate = '\u2067';

    /// <summary>U+2068, first-strong isolate.</summary>
    public const char FirstStrongIsolate = '\u2068';

    /// <summary>U+2069, pop directional isolate.</summary>
    public const char PopDirectionalIsolate = '\u2069';

    /// <summary>
    /// The neutral characters that stay outside an isolated code: whitespace, Western and
    /// Arabic punctuation. A trailing full stop after a code belongs to the surrounding
    /// sentence, not to the code.
    /// </summary>
    private const string NeutralPunctuation = " \t\u00A0.,;:!?()[]{}<>\"'«»،؛۔";

    /// <summary>
    /// The base direction of a run: the first strong character decides (UAX #9 P2/P3), with
    /// <paramref name="fallback"/> for text that has no strong character at all — an empty
    /// field, a number, or only punctuation.
    /// </summary>
    public static TextDirection BaseDirection(string? text, TextDirection fallback = TextDirection.LeftToRight)
    {
        if (string.IsNullOrEmpty(text))
        {
            return fallback;
        }

        foreach (var rune in text.EnumerateRunes())
        {
            if (!IsLetter(rune))
            {
                // Digits, punctuation, marks and whitespace are weak: they never set the base
                // direction, which is why a bare "1,234.50" keeps the field's direction.
                continue;
            }

            return IsRightToLeftBlock(rune.Value) ? TextDirection.RightToLeft : TextDirection.LeftToRight;
        }

        return fallback;
    }

    /// <summary>
    /// The base direction a field of this kind must use. Code-like kinds are left-to-right
    /// whatever the surrounding language says; labels, prose and addresses follow their own
    /// text and fall back to the field's language direction.
    /// </summary>
    public static TextDirection FieldDirection(FieldTextKind kind, string? text, TextDirection fieldDirection) => kind switch
    {
        FieldTextKind.Code or FieldTextKind.InvoiceNumber or FieldTextKind.Iban or FieldTextKind.Phone
            or FieldTextKind.Email or FieldTextKind.Money or FieldTextKind.Date => TextDirection.LeftToRight,
        FieldTextKind.Label or FieldTextKind.FreeText or FieldTextKind.Address =>
            BaseDirection(text, fieldDirection),
        _ => throw new ArgumentOutOfRangeException(nameof(kind), kind, "Unknown field text kind."),
    };

    /// <summary>The Avalonia flow direction for a base direction.</summary>
    public static FlowDirection Flow(TextDirection direction) =>
        direction == TextDirection.RightToLeft ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;

    /// <summary>The base direction for an Avalonia flow direction.</summary>
    public static TextDirection Direction(FlowDirection flow) =>
        flow == FlowDirection.RightToLeft ? TextDirection.RightToLeft : TextDirection.LeftToRight;

    /// <summary>Wraps a value in a fixed-direction isolate (U+2066/U+2067 … U+2069).</summary>
    public static string Isolate(string? value, TextDirection direction)
    {
        if (value is null)
        {
            return string.Empty;
        }

        var opener = direction == TextDirection.RightToLeft ? RightToLeftIsolate : LeftToRightIsolate;
        return string.Create(value.Length + 2, (value, opener), static (span, state) =>
        {
            span[0] = state.opener;
            state.value.CopyTo(span[1..]);
            span[^1] = PopDirectionalIsolate;
        });
    }

    /// <summary>Wraps a value in a first-strong isolate (U+2068 … U+2069).</summary>
    public static string IsolateAuto(string? value)
    {
        if (value is null)
        {
            return string.Empty;
        }

        var builder = new StringBuilder(value.Length + 2);
        builder.Append(FirstStrongIsolate).Append(value).Append(PopDirectionalIsolate);
        return builder.ToString();
    }

    /// <summary>
    /// The display form of a code-like value: the code isolated left-to-right, with leading and
    /// trailing neutral punctuation kept outside the isolate. Without that split a trailing
    /// full stop after an isolated code is resolved by the surrounding right-to-left paragraph
    /// and jumps to the other end of the line.
    /// </summary>
    public static string IsolateCode(string? code)
    {
        if (string.IsNullOrEmpty(code))
        {
            return code ?? string.Empty;
        }

        var start = 0;
        var end = code.Length;
        while (start < end && IsNeutral(code[start]))
        {
            start++;
        }

        while (end > start && IsNeutral(code[end - 1]))
        {
            end--;
        }

        if (start == end)
        {
            return code;
        }

        var builder = new StringBuilder(code.Length + 2);
        builder.Append(code, 0, start);
        builder.Append(LeftToRightIsolate);
        builder.Append(code, start, end - start);
        builder.Append(PopDirectionalIsolate);
        builder.Append(code, end, code.Length - end);
        return builder.ToString();
    }

    /// <summary>
    /// The string to render for a field of a given kind. Code-like kinds are isolated; prose
    /// is returned unchanged because the bidi algorithm plus the field's own
    /// <see cref="FlowDirection"/> (from <see cref="FieldDirection"/>) lay it out correctly.
    /// The returned string is for display only — never store it, and strip isolates with
    /// <see cref="RemoveIsolates"/> before comparing or copying.
    /// </summary>
    public static string ForDisplay(string? value, FieldTextKind kind, TextDirection fieldDirection)
    {
        if (string.IsNullOrEmpty(value))
        {
            return value ?? string.Empty;
        }

        return kind switch
        {
            FieldTextKind.Code or FieldTextKind.InvoiceNumber or FieldTextKind.Iban or FieldTextKind.Phone
                or FieldTextKind.Email or FieldTextKind.Money or FieldTextKind.Date => IsolateCode(value),
            FieldTextKind.Label or FieldTextKind.FreeText or FieldTextKind.Address => value,
            _ => throw new ArgumentOutOfRangeException(nameof(kind), kind, "Unknown field text kind."),
        };
    }

    /// <summary>Removes every isolate and embedding/override character, restoring the value.</summary>
    public static string RemoveIsolates(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        var builder = new StringBuilder(text.Length);
        foreach (var character in text)
        {
            if (IsFormattingControl(character))
            {
                continue;
            }

            builder.Append(character);
        }

        return builder.ToString();
    }

    /// <summary>True when the text carries an isolate or another bidi formatting control.</summary>
    public static bool ContainsFormattingControls(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return false;
        }

        foreach (var character in text)
        {
            if (IsFormattingControl(character))
            {
                return true;
            }
        }

        return false;
    }

    /// <summary>True when the text has Arabic-script letters (Urdu, Arabic, Persian).</summary>
    public static bool HasArabicScript(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return false;
        }

        foreach (var rune in text.EnumerateRunes())
        {
            if (IsLetter(rune) && InArabicBlock(rune.Value))
            {
                return true;
            }
        }

        return false;
    }

    private static bool IsNeutral(char character) =>
        NeutralPunctuation.Contains(character);

    private static bool IsLetter(Rune rune) =>
        Rune.GetUnicodeCategory(rune) is UnicodeCategory.UppercaseLetter
            or UnicodeCategory.LowercaseLetter
            or UnicodeCategory.TitlecaseLetter
            or UnicodeCategory.ModifierLetter
            or UnicodeCategory.OtherLetter;

    /// <summary>
    /// The right-to-left strong code-point blocks (UAX #9 strong types R and AL). Kept as
    /// ranges on purpose: the bidi category is not exposed by the BCL, and this list is what
    /// the first-strong rule needs — Arabic-script blocks, Hebrew, Syriac, Thaana, NKo,
    /// Samaritan, Mandaic and Adlam.
    /// </summary>
    private static bool IsRightToLeftBlock(int codePoint) => codePoint
        is >= 0x0590 and <= 0x05FF // Hebrew
        or >= 0x0600 and <= 0x06FF // Arabic
        or >= 0x0700 and <= 0x074F // Syriac
        or >= 0x0750 and <= 0x077F // Arabic Supplement
        or >= 0x0780 and <= 0x07BF // Thaana
        or >= 0x07C0 and <= 0x07FF // NKo
        or >= 0x0800 and <= 0x083F // Samaritan
        or >= 0x0840 and <= 0x085F // Mandaic
        or >= 0x08A0 and <= 0x08FF // Arabic Extended-A
        or >= 0xFB1D and <= 0xFB4F // Hebrew presentation forms
        or >= 0xFB50 and <= 0xFDFF // Arabic Presentation Forms-A
        or >= 0xFE70 and <= 0xFEFF // Arabic Presentation Forms-B
        or >= 0x1E900 and <= 0x1E95F; // Adlam

    private static bool InArabicBlock(int codePoint) => codePoint
        is >= 0x0600 and <= 0x06FF
        or >= 0x0750 and <= 0x077F
        or >= 0x08A0 and <= 0x08FF
        or >= 0xFB50 and <= 0xFDFF
        or >= 0xFE70 and <= 0xFEFF;

    private static bool IsFormattingControl(char character) => character
        is >= '\u202A' and <= '\u202E' // LRE, RLE, PDF, LRO, RLO
        or >= '\u2066' and <= '\u2069' // LRI, RLI, FSI, PDI
        or >= '\u200E' and <= '\u200F' // LRM, RLM
        or '\u061C';                   // ALM
}
