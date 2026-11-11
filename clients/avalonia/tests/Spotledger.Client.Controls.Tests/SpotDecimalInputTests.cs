using System.Globalization;
using System.Reflection;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

/// <summary>Money-critical decimal behaviour: exact values, scale rejection, bounds, formats.</summary>
public sealed class SpotDecimalInputTests
{
    private static readonly SpotNumberFormat Default = new(",", ".");
    private static readonly SpotNumberFormat European = new(".", ",");

    [AvaloniaFact]
    public void TypingDigitsAndSeparatorCommitsScaledValue()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        window.KeyTextInput("1234.5");
        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(1234.50m, input.Value);
        // Equality ignores decimal scale: money must carry exactly Scale fraction digits,
        // not the shape it happened to be typed in.
        Assert.Equal(2, input.Value!.Value.Scale);
        Assert.Equal("1234.50", input.Value!.Value.ToString("F2", CultureInfo.InvariantCulture));
        Assert.Null(input.ErrorText);
        Assert.Equal("1,234.50", input.Text);
    }

    [AvaloniaFact]
    public void ScaleOverflowIsRejectedAndKeepsOldValue()
    {
        var input = new SpotDecimalInput { Scale = 2, Value = 100m };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        input.Text = string.Empty;
        window.KeyTextInput("12.345");
        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(100m, input.Value);
        Assert.NotNull(input.ErrorText);
        Assert.Contains("decimal place", input.ErrorText, StringComparison.Ordinal);
        Assert.Equal("100.00", input.Text);
    }

    [AvaloniaFact]
    public void NegativeZeroNormalisesToZero()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        TestHost.Show(input);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        input.Text = "-0";
        input.Commit();

        Assert.Equal(0m, input.Value);
        Assert.Equal("0.00", input.DisplayText(input.Value));
        Assert.DoesNotContain('-', input.Text);
    }

    [AvaloniaFact]
    public void BoundsMinAndMaxRejectOutsideAndAcceptEdges()
    {
        var input = new SpotDecimalInput { Scale = 2, Min = 1m, Max = 99m };
        TestHost.Show(input);

        input.Text = "0.5";
        Assert.False(input.Commit());
        Assert.Null(input.Value);
        Assert.NotNull(input.ErrorText);

        input.Text = "1";
        Assert.True(input.Commit());
        Assert.Equal(1m, input.Value);

        input.Text = "99";
        Assert.True(input.Commit());
        Assert.Equal(99m, input.Value);

        input.Text = "99.01";
        Assert.False(input.Commit());
        Assert.Equal(99m, input.Value);
    }

    [AvaloniaFact]
    public void PasteWithGroupingSeparatorsStripAndParse()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        TestHost.Show(input);

        // Clipboard on the headless platform is a stub, so the paste result is delivered
        // as the field text; Commit is what every paste path ends in.
        input.Text = "1,234.50";
        Assert.True(input.Commit());
        Assert.Equal(1234.50m, input.Value);
    }

    [AvaloniaFact]
    public void TypingNonNumericCharactersAreDropped()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        window.KeyTextInput("a1b2,3.4x5");
        Assert.Equal("123.45", input.Text);
        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(123.45m, input.Value);
    }

    [AvaloniaFact]
    public void CustomNumberFormatGroupingDotDecimalComma()
    {
        var input = new SpotDecimalInput { Scale = 2, NumberFormat = European };
        TestHost.Show(input);

        Assert.Equal("1.234,50", input.DisplayText(1234.50m));
        Assert.Equal("1234,50", input.EditText(1234.50m));

        input.Text = "1.234,50";
        Assert.True(input.Commit());
        Assert.Equal(1234.50m, input.Value);
    }

    [AvaloniaFact]
    public void BoundOverScaleValueSurvivesFocusAndBlurWithoutRounding()
    {
        // Lead review #431 item 1: focusing a field whose bound value carries more fraction
        // digits than Scale must never round it — the display shows the value verbatim and
        // the blur commit rejects (not rounds), leaving Value untouched.
        var input = new SpotDecimalInput { Scale = 2, Value = 1.005m };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal("1.005", input.Text); // resting display is not rounded either

        input.Focus();
        Dispatcher.UIThread.RunJobs();
        Assert.Equal("1.005", input.Text);

        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(1.005m, input.Value);
        Assert.Equal(3, input.Value!.Value.Scale);
    }

    [AvaloniaFact]
    public void TypingOverSelectAllReplacesTheSeparatorInTheSelection()
    {
        // Lead review #431 item 2: "12.50" selected + typed "3.5" must give "3.5", not "35".
        var input = new SpotDecimalInput { Scale = 2, Value = 12.50m };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        Assert.Equal("12.50", input.Text);
        input.SelectAll();

        window.KeyTextInput("3.5");
        Assert.Equal("3.5", input.Text);

        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(3.50m, input.Value);
        Assert.Equal(2, input.Value!.Value.Scale);
    }

    [AvaloniaFact]
    public void TypingMinusOverASelectionThatIncludesTheMinusReplacesIt()
    {
        var input = new SpotDecimalInput { Scale = 2, Value = -12.50m };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        Assert.Equal("-12.50", input.Text);
        input.SelectAll();

        window.KeyTextInput("-7");
        Assert.Equal("-7", input.Text);

        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(-7.00m, input.Value);
        Assert.Equal(2, input.Value!.Value.Scale);
    }

    [AvaloniaFact]
    public void PastedGroupingOverASelectionKeepsItsDecimalSeparator()
    {
        // Same filter path as a paste over a selection: "1,234.50" over "12.50" gives 1234.50.
        var input = new SpotDecimalInput { Scale = 2, Value = 12.50m };
        var host = new StackPanel();
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        input.SelectAll();

        window.KeyTextInput("1,234.50");
        Assert.Equal("1234.50", input.Text);

        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(1234.50m, input.Value);
        Assert.Equal(2, input.Value!.Value.Scale);
    }

    [AvaloniaTheory]
    [InlineData("1234.5", 2, 2)]
    [InlineData("100", 2, 2)]
    [InlineData("0.5", 2, 2)]
    [InlineData("7", 0, 0)]
    [InlineData("-1.5", 2, 2)]
    public void CommittedValueCarriesTheScale(string typed, int scale, int expectedScale)
    {
        // Lead review #431 item 3: the committed decimal must carry exactly Scale fraction
        // digits, not the shape it happened to be typed in.
        var input = new SpotDecimalInput { Scale = scale };
        TestHost.Show(input);
        input.Text = typed;
        Assert.True(input.Commit());
        Assert.Equal(expectedScale, input.Value!.Value.Scale);
    }

    [AvaloniaFact]
    public void RoundTripThroughDisplayAndEditNeverChangesValue()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        TestHost.Show(input);
        var random = new Random(20260930);

        for (var i = 0; i < 300; i++)
        {
            var cents = random.Next(-5_000_000, 5_000_000);
            var baseValue = cents / 100m;

            // Lead review #431: mix in unscaled, under-scaled and over-scale values —
            // cent-derived values alone all sit exactly at scale 2.
            var original = (i % 4) switch
            {
                0 => baseValue, // scale 0..2
                1 => decimal.Truncate(baseValue), // unscaled
                2 => decimal.Round(baseValue, 1), // under-scaled
                _ => baseValue + 0.001m * random.Next(1, 10), // over-scale: never rounded here
            };
            var overScale = original.Scale > 2;
            input.Value = original;

            // display -> commit
            input.Text = input.DisplayText(original);
            if (overScale)
            {
                Assert.False(input.Commit(), $"over-scale display round trip must reject for {original}");
                Assert.Equal(original, input.Value);
                Assert.Equal(original.Scale, input.Value!.Value.Scale);
            }
            else
            {
                Assert.True(input.Commit(), $"display round trip failed for {original}: error {input.ErrorText}");
                Assert.Equal(original, input.Value);
            }

            // edit -> commit
            input.Text = input.EditText(original);
            if (overScale)
            {
                Assert.False(input.Commit(), $"over-scale edit round trip must reject for {original}");
                Assert.Equal(original, input.Value);
                Assert.Equal(original.Scale, input.Value!.Value.Scale);
            }
            else
            {
                Assert.True(input.Commit(), $"edit round trip failed for {original}");
                Assert.Equal(original, input.Value);
                Assert.Equal(2, input.Value!.Value.Scale); // money carries the field scale
            }
        }
    }

    [AvaloniaFact]
    public void EmptyFieldCommitsNull()
    {
        var input = new SpotDecimalInput { Scale = 2, Value = 7m };
        TestHost.Show(input);
        input.Text = string.Empty;
        Assert.True(input.Commit());
        Assert.Null(input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void ScaleZeroRejectsAnyFraction()
    {
        var input = new SpotDecimalInput { Scale = 0 };
        TestHost.Show(input);
        input.Text = "12.5";
        Assert.False(input.Commit());
        Assert.Null(input.Value);

        input.Text = "12";
        Assert.True(input.Commit());
        Assert.Equal(12m, input.Value);
    }

    [AvaloniaFact]
    public void ValueIsDecimalNotDouble()
    {
        Assert.Equal(typeof(decimal?), SpotDecimalInput.ValueProperty.PropertyType);
        Assert.Equal(typeof(decimal?), SpotDecimalInput.MinProperty.PropertyType);
        Assert.Equal(typeof(decimal?), SpotDecimalInput.MaxProperty.PropertyType);
        Assert.DoesNotContain(
            typeof(SpotDecimalInput).GetProperties(BindingFlags.DeclaredOnly | BindingFlags.Public | BindingFlags.Instance),
            p => p.PropertyType == typeof(double) || p.PropertyType == typeof(double?));
    }

    // ---------------------------------------------------------------------------------------
    // Eastern digits and Central/Eastern keyboards (WP-UI-15.T5). The box always holds ASCII:
    // an Eastern digit is mapped on the way in, so what the user sees while editing is Latin,
    // and the value committed is the same decimal an ASCII typist would get.
    // ---------------------------------------------------------------------------------------

    /// <summary>U+0660..U+0669.</summary>
    private const string ArabicIndicDigits = "\u0660\u0661\u0662\u0663\u0664\u0665\u0666\u0667\u0668\u0669";

    /// <summary>U+06F0..U+06F9 (Persian/Urdu keyboards).</summary>
    private const string ExtendedArabicIndicDigits = "\u06F0\u06F1\u06F2\u06F3\u06F4\u06F5\u06F6\u06F7\u06F8\u06F9";

    [AvaloniaFact]
    public void EveryArabicIndicDigitTypedOneByOneBecomesTheAsciiDigit()
    {
        var input = new SpotDecimalInput { Scale = 0 };
        var window = ShowFocused(input);

        foreach (var digit in ArabicIndicDigits)
        {
            window.KeyTextInput(digit.ToString());
        }

        // One typed character, one character in the box: nothing vanishes, nothing doubles.
        Assert.Equal("0123456789", input.Text);
        Assert.True(input.Commit());
        Assert.Equal(123456789m, input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void EveryExtendedArabicIndicDigitTypedOneByOneBecomesTheAsciiDigit()
    {
        var input = new SpotDecimalInput { Scale = 0 };
        var window = ShowFocused(input);

        foreach (var digit in ExtendedArabicIndicDigits)
        {
            window.KeyTextInput(digit.ToString());
        }

        Assert.Equal("0123456789", input.Text);
        Assert.True(input.Commit());
        Assert.Equal(123456789m, input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void TypedArabicIndicNumberWithTheArabicDecimalSeparatorCommitsTheSameDecimal()
    {
        // ١٢٣٫٤٥ — U+066B between the integer and the fraction part.
        var input = new SpotDecimalInput { Scale = 2 };
        var window = ShowFocused(input);

        foreach (var character in "\u0661\u0662\u0663\u066B\u0664\u0665")
        {
            window.KeyTextInput(character.ToString());
        }

        Assert.Equal("123.45", input.Text);
        Assert.True(input.Commit());
        Assert.Equal(123.45m, input.Value);
        Assert.Equal(2, input.Value!.Value.Scale);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void TypedExtendedArabicIndicNumberWithTheArabicDecimalSeparatorCommitsTheSameDecimal()
    {
        // ۱۲۳٫۴۵ — the Urdu keyboard's digits plus U+066B.
        var input = new SpotDecimalInput { Scale = 2 };
        var window = ShowFocused(input);

        foreach (var character in "\u06F1\u06F2\u06F3\u066B\u06F4\u06F5")
        {
            window.KeyTextInput(character.ToString());
        }

        Assert.Equal("123.45", input.Text);
        Assert.True(input.Commit());
        Assert.Equal(123.45m, input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void TypedPersianMinusBecomesTheMinusAndKeepsTheSign()
    {
        // U+2212 is what a Persian keyboard emits for minus.
        var input = new SpotDecimalInput { Scale = 2 };
        var window = ShowFocused(input);

        window.KeyTextInput("\u2212");
        window.KeyTextInput("\u06F7");

        Assert.Equal("-7", input.Text);
        Assert.True(input.Commit());
        Assert.Equal(-7.00m, input.Value);
    }

    [AvaloniaFact]
    public void PastedExtendedArabicIndicGroupedNumberCommits()
    {
        // ۱٬۲۳۴٫۵ — U+066C grouping, U+066B decimal. Clipboard on the headless platform is a
        // stub, so the paste result is delivered as the field text; Commit is what every paste
        // path ends in.
        var input = new SpotDecimalInput { Scale = 2 };
        TestHost.Show(input);

        input.Text = "\u06F1\u066C\u06F2\u06F3\u06F4\u066B\u06F5";
        Assert.True(input.Commit());
        Assert.Equal(1234.5m, input.Value);
        Assert.Equal(2, input.Value!.Value.Scale);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void PastedSouthAsianGroupingIsAccepted()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        TestHost.Show(input);

        input.Text = "12,34,567.89";
        Assert.True(input.Commit());
        Assert.Equal(1234567.89m, input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void PastedAmbiguousSeparatorIsRefusedWithItsMessageAndKeepsTheValue()
    {
        // 1,5 could be 1.5 or 15. Never guessed: refused, with the catalogue key as the message
        // and the last good value intact.
        var input = new SpotDecimalInput { Scale = 2, Value = 10m };
        TestHost.Show(input);

        input.Text = "1,5";
        Assert.False(input.Commit());
        Assert.Equal("numeric.ambiguous_separator", input.ErrorText);
        Assert.Equal(10m, input.Value);
        Assert.Equal("10.00", input.Text); // the last good value, restored
    }

    [AvaloniaFact]
    public void AmbiguousPasteThroughTheTextInputPathIsRefusedInsteadOfBecomingFifteen()
    {
        // The same refusal on the insert path: a multi-character text input (a paste) whose
        // separators are ambiguous inserts nothing and says why, instead of dropping the comma.
        var input = new SpotDecimalInput { Scale = 2, Value = 7m };
        var window = ShowFocused(input);
        input.SelectAll();

        window.KeyTextInput("1,5");

        Assert.Equal("7.00", input.Text);
        Assert.Equal("numeric.ambiguous_separator", input.ErrorText);
        Assert.Equal(7m, input.Value);
    }

    [AvaloniaFact]
    public void CaretFollowsTheInsertedDigitWhenTypingInsideANumberInARightToLeftWindow()
    {
        // RTL changes where the glyphs sit, never the logical offsets: typing one Eastern digit
        // in the middle of "12.50" inserts exactly one character and leaves the caret after it.
        var input = new SpotDecimalInput { Scale = 2, Value = 12.50m };
        var window = ShowFocused(input, FlowDirection.RightToLeft);
        Assert.Equal(FlowDirection.RightToLeft, input.FlowDirection);
        Assert.Equal("12.50", input.Text);

        input.CaretIndex = 2;
        window.KeyTextInput("\u06F7"); // ۷

        Assert.Equal("127.50", input.Text);
        Assert.Equal(3, input.CaretIndex);
        Assert.Equal(0, input.SelectionEnd - input.SelectionStart);

        TestHost.ClearFocus(input);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(127.50m, input.Value);
    }

    [AvaloniaFact]
    public void BoundsStillApplyAfterNormalisation()
    {
        var input = new SpotDecimalInput { Scale = 2, Min = 1m, Max = 1000m };
        TestHost.Show(input);

        input.Text = "\u06F1\u06F2\u06F3\u066B\u06F4\u06F5"; // ۱۲۳٫۴۵
        Assert.True(input.Commit());
        Assert.Equal(123.45m, input.Value);

        input.Text = "\u06F1\u066C\u06F2\u06F3\u06F4\u066B\u06F5"; // ۱٬۲۳۴٫۵ — over Max
        Assert.False(input.Commit());
        Assert.Equal(123.45m, input.Value);
        Assert.Contains("at most", input.ErrorText, StringComparison.Ordinal);

        input.Text = "\u0660\u066B\u0665"; // ٠٫٥ — under Min
        Assert.False(input.Commit());
        Assert.Equal(123.45m, input.Value);
        Assert.Contains("at least", input.ErrorText, StringComparison.Ordinal);
    }

    /// <summary>Shows <paramref name="input"/> in a panel of the given direction and focuses it.</summary>
    private static Window ShowFocused(SpotDecimalInput input, FlowDirection direction = FlowDirection.LeftToRight)
    {
        var host = new StackPanel { FlowDirection = direction };
        host.Children.Add(input);
        var window = TestHost.Show(host);
        input.Focus();
        Dispatcher.UIThread.RunJobs();
        return window;
    }
}
