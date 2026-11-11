using System.Globalization;
using System.Text;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Data;
using Avalonia.Input;
using Avalonia.Interactivity;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Controls;

/// <summary>
/// A money- and quantity-critical numeric entry. The value is a <see cref="decimal"/> — never
/// a double (ADR-12). Display groups thousands using <see cref="NumberFormat"/>; editing shows
/// the raw value. Input is restricted to digits, one decimal separator and a leading minus;
/// pasted grouping separators are stripped, so "1,234.50" pastes as 1234.50. A scale overflow
/// or an out-of-range value is rejected with an error and the old value is kept — values are
/// never silently rounded (rounding rules belong to the server, WP-DOC-02). The separators
/// come from <see cref="NumberFormat"/>, never from the OS culture.
///
/// Eastern digits are accepted: a typed Arabic-Indic (<c>١٢٣</c>) or Extended Arabic-Indic
/// (<c>۱۲۳</c>) character becomes the ASCII digit in the box, U+066B becomes the control's
/// decimal separator and U+2212 becomes the minus, one typed character in and one character
/// out, so the caret and the selection move exactly as they do for ASCII typing. Pasted and
/// committed text goes through <see cref="NumeralText.TryNormalizeNumericInput"/>; a separator
/// that could be read two ways (<c>1,5</c>) or a misplaced grouping separator is refused with
/// <see cref="NumeralText.Explain"/>'s message and the last good value is kept instead of
/// committing a guess. Display is unchanged: the digit set a value is <em>shown</em> with is a
/// separate setting and not this control's business.
/// </summary>
public class SpotDecimalInput : TextBox
{
    public static readonly StyledProperty<decimal?> ValueProperty =
        AvaloniaProperty.Register<SpotDecimalInput, decimal?>(
            nameof(Value), defaultBindingMode: BindingMode.TwoWay);

    public static readonly StyledProperty<int> ScaleProperty =
        AvaloniaProperty.Register<SpotDecimalInput, int>(nameof(Scale), 2);

    public static readonly StyledProperty<decimal?> MinProperty =
        AvaloniaProperty.Register<SpotDecimalInput, decimal?>(nameof(Min));

    public static readonly StyledProperty<decimal?> MaxProperty =
        AvaloniaProperty.Register<SpotDecimalInput, decimal?>(nameof(Max));

    public static readonly StyledProperty<SpotNumberFormat> NumberFormatProperty =
        AvaloniaProperty.Register<SpotDecimalInput, SpotNumberFormat>(
            nameof(NumberFormat), SpotNumberFormat.Default);

    public static readonly StyledProperty<string?> ErrorTextProperty =
        AvaloniaProperty.Register<SpotDecimalInput, string?>
            (nameof(ErrorText));

    private bool _suppressTextSync;

    /// <summary>The committed value; null when the field is empty.</summary>
    public decimal? Value
    {
        get => GetValue(ValueProperty);
        set => SetValue(ValueProperty, value);
    }

    /// <summary>Maximum allowed number of decimal places on commit.</summary>
    public int Scale
    {
        get => GetValue(ScaleProperty);
        set => SetValue(ScaleProperty, value);
    }

    /// <summary>Inclusive lower bound, or null for none.</summary>
    public decimal? Min
    {
        get => GetValue(MinProperty);
        set => SetValue(MinProperty, value);
    }

    /// <summary>Inclusive upper bound, or null for none.</summary>
    public decimal? Max
    {
        get => GetValue(MaxProperty);
        set => SetValue(MaxProperty, value);
    }

    /// <summary>The group and decimal separators used for display and parsing.</summary>
    public SpotNumberFormat NumberFormat
    {
        get => GetValue(NumberFormatProperty);
        set => SetValue(NumberFormatProperty, value);
    }

    /// <summary>The validation message after a rejected commit; cleared on success.</summary>
    public string? ErrorText
    {
        get => GetValue(ErrorTextProperty);
        set => SetValue(ErrorTextProperty, value);
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ValueProperty && !_suppressTextSync && !IsFocused)
        {
            SyncTextToState();
        }
        else if (change.Property == ErrorTextProperty)
        {
            PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        }
        else if ((change.Property == ScaleProperty || change.Property == NumberFormatProperty) && !IsFocused)
        {
            SyncTextToState();
        }
    }

    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        base.OnApplyTemplate(e);
        PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        if (!IsFocused)
        {
            SyncTextToState();
        }
    }

    protected override void OnGotFocus(Avalonia.Input.FocusChangedEventArgs e)
    {
        base.OnGotFocus(e);
        SetText(EditText(Value));
    }

    protected override void OnLostFocus(Avalonia.Input.FocusChangedEventArgs e)
    {
        base.OnLostFocus(e);
        Commit();
    }

    /// <summary>
    /// Filters typed and pasted input to digits, one decimal separator and a leading minus.
    /// Each character is first mapped onto the editor's alphabet (see
    /// <see cref="MapTypedCharacter"/>), so Eastern digits become ASCII digits in the box. Group
    /// separators are accepted but dropped, so a pasted "1,234.50" becomes "1234.50" — except
    /// for a multi-character input whose separators cannot be read unambiguously: that is
    /// refused with <see cref="ErrorText"/> set, because dropping the separator would turn
    /// "1,5" into fifteen.
    /// </summary>
    protected override void OnTextInput(TextInputEventArgs e)
    {
        if (!e.Handled && !IsReadOnly && e.Text is { Length: > 0 } input)
        {
            var mapped = MapTypedText(input);
            if (mapped.Length > 1)
            {
                // A paste arrives here as one multi-character text input. The normaliser decides
                // whether its separators are well formed; guesses are refused, never filtered
                // into a different number.
                var canonical = NumeralText.TryNormalizeNumericInput(ToCanonicalNumericText(mapped));
                if (canonical.Problem is NumericInputProblem.MisplacedGrouping or NumericInputProblem.AmbiguousSeparator)
                {
                    ErrorText = NumeralText.Explain(canonical.Problem);
                    e.Handled = true;
                    return;
                }

                if (canonical.IsValid && canonical.Normalized.Length > 0)
                {
                    mapped = canonical.Normalized.Replace('.', DecimalChar);
                }
            }

            var selectionStart = SelectionStart;
            var selectionLength = Math.Max(0, SelectionEnd - selectionStart);
            var filtered = FilterEditableChars(mapped, Text ?? string.Empty, selectionStart, selectionLength);
            if (filtered.Length == 0)
            {
                e.Handled = true;
                return;
            }

            if (!string.Equals(filtered, input, StringComparison.Ordinal))
            {
                e.Text = filtered;
            }
        }

        base.OnTextInput(e);
    }

    /// <summary>
    /// Parses and validates the current edit text. Empty commits as null. An ambiguous or
    /// misplaced separator is refused with <see cref="NumeralText.Explain"/>'s message; a scale
    /// overflow, an unparseable value or an out-of-range value sets <see cref="ErrorText"/>,
    /// restores the last good text and keeps the previous value. Eastern digits and the Arabic
    /// separators are normalised to the canonical Latin edit form before the parse, so
    /// <c>١٢٣٫٤٥</c> and <c>۱۲۳٫۴۵</c> commit as 123.45. "-0" normalises to 0.
    /// </summary>
    public bool Commit()
    {
        var raw = (Text ?? string.Empty).Trim();
        if (raw.Length == 0 || raw == "-" || raw == "\u2212")
        {
            CommitValue(null, clearError: true);
            return true;
        }

        var normalised = NumeralText.TryNormalizeNumericInput(ToCanonicalNumericText(raw));
        if (normalised.Problem is NumericInputProblem.MisplacedGrouping or NumericInputProblem.AmbiguousSeparator)
        {
            // The separator could be read two ways (1,5) or sits where grouping cannot go:
            // refuse instead of committing a guess, quoting the catalogue message key.
            Reject(NumeralText.Explain(normalised.Problem));
            return false;
        }

        var invariant = normalised.Normalized;
        var scaleDigits = FractionDigits(invariant);
        if (!normalised.IsValid
            || !decimal.TryParse(invariant,
                NumberStyles.AllowLeadingSign | NumberStyles.AllowDecimalPoint,
                CultureInfo.InvariantCulture, out var parsed))
        {
            Reject("Enter a valid number.");
            return false;
        }

        if (scaleDigits > Scale)
        {
            Reject($"At most {Scale} decimal place{(Scale == 1 ? string.Empty : "s")} are allowed here.");
            return false;
        }

        if (parsed == 0m)
        {
            parsed = 0m; // normalises "-0" to "0"
        }

        if (Min.HasValue && parsed < Min.Value)
        {
            Reject($"Value must be at least {DisplayText(Min.Value)}.");
            return false;
        }

        if (Max.HasValue && parsed > Max.Value)
        {
            Reject($"Value must be at most {DisplayText(Max.Value)}.");
            return false;
        }

        CommitValue(NormaliseScale(parsed), clearError: true);
        return true;
    }

    /// <summary>
    /// Pads the committed decimal to <see cref="Scale"/> fraction digits so a money value does
    /// not reach the server in the shape it happened to be typed in. Safe only after the scale
    /// overflow check: this never drops digits, it only appends zeros.
    /// </summary>
    private decimal NormaliseScale(decimal value)
    {
        var text = value.ToString("F" + Scale.ToString(CultureInfo.InvariantCulture), CultureInfo.InvariantCulture);
        return decimal.Parse(text, NumberStyles.AllowLeadingSign | NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture);
    }

    /// <summary>Grouped display text for a value ("1,234.50"); empty for null.</summary>
    public string DisplayText(decimal? value)
    {
        if (value is not { } v)
        {
            return string.Empty;
        }

        return Group(EditText(v));
    }

    /// <summary>
    /// Raw editable text for a value ("1234.50" in the default format); empty for null. A value
    /// that carries more fraction digits than <see cref="Scale"/> is shown unrounded and verbatim
    /// — never rounded to the scale (rounding decisions belong to the server, WP-DOC-02), so
    /// focusing and blurring cannot rewrite a bound value.
    /// </summary>
    public string EditText(decimal? value)
    {
        if (value is not { } v)
        {
            return string.Empty;
        }

        if (v == 0m)
        {
            v = 0m; // kills "-0"
        }

        var scale = Scale.ToString(CultureInfo.InvariantCulture);
        // F{scale} would silently round an over-scale value; use F0 plus the stored fraction
        // digits so every digit is rendered exactly.
        var digits = Math.Max(v.Scale, Scale);
        var text = digits > Scale
            ? v.ToString("F" + digits.ToString(CultureInfo.InvariantCulture), CultureInfo.InvariantCulture)
            : v.ToString("F" + scale, CultureInfo.InvariantCulture);
        return NumberFormat.DecimalSeparator == "."
            ? text
            : text.Replace('.', formatDecimalChar());

        char formatDecimalChar()
        {
            var separator = NumberFormat.DecimalSeparator;
            return separator.Length > 0 ? separator[0] : '.';
        }
    }

    private void CommitValue(decimal? value, bool clearError)
    {
        _suppressTextSync = true;
        try
        {
            Value = value;
            if (clearError)
            {
                ErrorText = null;
            }

            SetText(IsFocused ? EditText(value) : DisplayText(value));
        }
        finally
        {
            _suppressTextSync = false;
        }
    }

    private void Reject(string message)
    {
        ErrorText = message;
        SetText(DisplayText(Value));
    }

    private void SyncTextToState()
    {
        SetText(IsFocused ? EditText(Value) : DisplayText(Value));
    }

    private void SetText(string text)
    {
        _suppressTextSync = true;
        try
        {
            Text = text;
        }
        finally
        {
            _suppressTextSync = false;
        }
    }

    /// <summary>
    /// Keeps only characters the editor accepts: ASCII digits (the caller has already mapped the
    /// Eastern digit sets onto them), the decimal separator (once, normalised to one character),
    /// a leading minus (only when the text the user is replacing keeps none), and group
    /// separators, which are accepted but dropped. The "does the text already have a
    /// separator/minus?" check runs on the text with the current selection removed, because that
    /// selection is about to be replaced by the typed characters.
    /// </summary>
    private string FilterEditableChars(string input, string existingText, int selectionStart, int selectionLength)
    {
        var decimalChar = DecimalChar;
        var groupChar = GroupChar;
        selectionStart = Math.Clamp(selectionStart, 0, existingText.Length);
        selectionLength = Math.Clamp(selectionLength, 0, existingText.Length - selectionStart);
        var remaining = existingText.Remove(selectionStart, selectionLength);
        var hasDecimal = remaining.Contains(decimalChar);
        var hasMinus = remaining.Contains('-', StringComparison.Ordinal);
        var builder = new StringBuilder(input.Length);

        foreach (var ch in input)
        {
            if (char.IsAsciiDigit(ch))
            {
                builder.Append(ch);
                continue;
            }

            if (ch == decimalChar)
            {
                if (Scale > 0 && !hasDecimal)
                {
                    builder.Append(decimalChar);
                    hasDecimal = true;
                }

                continue;
            }

            if (groupChar is { } g && ch == g)
            {
                continue; // paste grouping separators are dropped
            }

            if (ch == '-' && !hasMinus && selectionStart == 0 && builder.Length == 0)
            {
                builder.Append('-');
                hasMinus = true;
            }

            // Anything else (letters, spaces, second separators, misplaced minus) is dropped.
        }

        return builder.ToString();
    }

    private char DecimalChar => NumberFormat.DecimalSeparator.Length > 0 ? NumberFormat.DecimalSeparator[0] : '.';

    private char? GroupChar => NumberFormat.GroupSeparator.Length > 0 ? NumberFormat.GroupSeparator[0] : null;

    /// <summary>
    /// Maps the control's own separators onto the normaliser's alphabet — the decimal separator
    /// to "." and the grouping separator to "," — so a European-configured field (group ".",
    /// decimal ",") reads "1.234,50" the same way a default one reads "1,234.50". Order-safe:
    /// one pass, no chained <see cref="string.Replace(string,string)"/> that could undo itself.
    /// Eastern digits are left to <see cref="NumeralText"/>.
    /// </summary>
    private string ToCanonicalNumericText(string raw)
    {
        var decimalChar = DecimalChar;
        var groupChar = GroupChar;
        var builder = new StringBuilder(raw.Length);
        foreach (var ch in raw)
        {
            if (ch == decimalChar)
            {
                builder.Append('.');
            }
            else if (groupChar is { } g && ch == g)
            {
                builder.Append(',');
            }
            else
            {
                builder.Append(ch);
            }
        }

        return builder.ToString();
    }

    /// <summary>
    /// Maps a typed or pasted string one character at a time, so the length never changes for a
    /// keystroke: the caret, the selection and the undo buffer behave exactly as they do for
    /// ASCII typing.
    /// </summary>
    private string MapTypedText(string input)
    {
        if (!NumeralText.ContainsEasternDigits(input)
            && input.IndexOf(NumeralText.ArabicDecimalSeparator) < 0
            && input.IndexOf('\u2212') < 0
            && input.IndexOf(NumeralText.ArabicThousandsSeparator) < 0
            && input.IndexOf('\u060C') < 0)
        {
            return input;
        }

        var builder = new StringBuilder(input.Length);
        foreach (var ch in input)
        {
            builder.Append(MapTypedCharacter(ch));
        }

        return builder.ToString();
    }

    /// <summary>
    /// One typed character onto the editor's alphabet: Arabic-Indic (U+0660-U+0669) and Extended
    /// Arabic-Indic (U+06F0-U+06F9) digits become ASCII digits, U+066B becomes the decimal
    /// separator this control edits with, U+2212 becomes the minus, and the Arabic thousands
    /// separators become ",". Always one character out for one in.
    /// </summary>
    private char MapTypedCharacter(char character) => character switch
    {
        >= '\u0660' and <= '\u0669' => (char)('0' + (character - '\u0660')),
        >= '\u06F0' and <= '\u06F9' => (char)('0' + (character - '\u06F0')),
        NumeralText.ArabicDecimalSeparator => DecimalChar,
        '\u2212' => '-',
        NumeralText.ArabicThousandsSeparator => GroupChar ?? ',',
        '\u060C' => GroupChar ?? ',',
        _ => character,
    };

    private static int FractionDigits(string invariant)
    {
        var index = invariant.IndexOf('.', StringComparison.Ordinal);
        return index < 0 ? 0 : invariant.Length - index - 1;
    }

    private string Group(string edit)
    {
        var format = NumberFormat;
        if (format.GroupSeparator.Length == 0 || edit.Length == 0)
        {
            return edit;
        }

        var negative = edit.StartsWith('-');
        var body = negative ? edit[1..] : edit;
        var decimalSeparator = NumberFormat.DecimalSeparator.Length > 0 ? NumberFormat.DecimalSeparator : ".";
        var dot = body.IndexOf(decimalSeparator, StringComparison.Ordinal);
        var integerPart = dot < 0 ? body : body[..dot];
        var fractionPart = dot < 0 ? string.Empty : body[dot..];

        var builder = new StringBuilder(integerPart.Length + integerPart.Length / 3 + 1);
        var count = 0;
        for (var i = integerPart.Length - 1; i >= 0; i--)
        {
            if (count > 0 && count % 3 == 0)
            {
                builder.Append(format.GroupSeparator);
            }

            builder.Append(integerPart[i]);
            count++;
        }

        var chars = builder.ToString().ToCharArray();
        Array.Reverse(chars);
        return (negative ? "-" : string.Empty) + new string(chars) + fractionPart;
    }
}
