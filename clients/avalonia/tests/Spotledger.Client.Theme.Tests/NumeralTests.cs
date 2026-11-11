using System.Globalization;
using System.Text;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The numerals setting. The same amount typed in Latin, Arabic-Indic or Extended Arabic-Indic
/// digits commits as the same <see cref="decimal"/>, and the display of money follows the
/// setting without ever leaving the decimal type (ADR-12: no floating point for money).
/// </summary>
public sealed class NumeralTests
{
    private const decimal Amount = 1234.5m;

    [Theory]
    [InlineData("١٢٣٫٤٥", "123.45")]
    [InlineData("۱۲۳٫۴۵", "123.45")]
    [InlineData("123.45", "123.45")]
    [InlineData("1,234.45", "1234.45")]
    [InlineData("۱٬۲۳۴٫۴۵", "1234.45")]
    public void EveryDigitSetNormalisesToLatinEditText(string typed, string expectedEditText)
    {
        Assert.Equal(expectedEditText, NumeralText.NormalizeNumericInput(typed));
        Assert.True(NumeralText.TryParseDecimal(typed, out var value));
        Assert.Equal(decimal.Parse(expectedEditText, System.Globalization.CultureInfo.InvariantCulture), value);
    }

    /// <summary>
    /// The table of <c>docs/urdu-rtl.md</c> section 3: the same twelve inputs and the same
    /// results, so the document cannot drift from the code. A refusal returns the text the user
    /// typed, unchanged, so the input can quote it.
    /// </summary>
    [Theory]
    [InlineData("1,5", "1,5", NumericInputProblem.AmbiguousSeparator)]
    [InlineData("1,50", "1,50", NumericInputProblem.AmbiguousSeparator)]
    [InlineData("1,500", "1500", NumericInputProblem.None)]
    [InlineData("12,34,567", "1234567", NumericInputProblem.None)]
    [InlineData("1,2,3", "1,2,3", NumericInputProblem.MisplacedGrouping)]
    [InlineData("\u0661\u066C\u0662\u0663\u0664\u066B\u0665", "1234.5", NumericInputProblem.None)]
    [InlineData("\u06F1\u066C\u06F2\u06F3\u06F4\u066B\u06F5", "1234.5", NumericInputProblem.None)]
    [InlineData("1.234,50", "1.234,50", NumericInputProblem.AmbiguousSeparator)]
    [InlineData("1,", "1,", NumericInputProblem.MisplacedGrouping)]
    [InlineData("--5", "--5", NumericInputProblem.UnknownCharacter)]
    [InlineData("", "", NumericInputProblem.Empty)]
    [InlineData("abc", "abc", NumericInputProblem.UnknownCharacter)]
    public void AmbiguousSeparatorsAreRefusedInsteadOfGuessed(string typed, string expectedText, NumericInputProblem expectedProblem)
    {
        var result = NumeralText.TryNormalizeNumericInput(typed);

        Assert.Equal(expectedProblem, result.Problem);
        Assert.Equal(expectedText, result.Normalized);
        Assert.Equal(expectedProblem == NumericInputProblem.None, result.IsValid);

        // The old string API keeps the refusal: the original text, and no parse.
        Assert.Equal(expectedText, NumeralText.NormalizeNumericInput(typed));
        Assert.Equal(expectedProblem == NumericInputProblem.None, NumeralText.TryParseDecimal(typed, out _));
    }

    /// <summary>
    /// Grouping is accepted only where it is well formed: Western threes, or South Asian twos and
    /// a final three. Everything else is refused with a reason.
    /// </summary>
    [Theory]
    [InlineData("1,234", "1234", NumericInputProblem.None)]
    [InlineData("12,345,678", "12345678", NumericInputProblem.None)]
    [InlineData("1,00,000", "100000", NumericInputProblem.None)]
    [InlineData("1234", "1234", NumericInputProblem.None)]
    [InlineData("1 234", "1234", NumericInputProblem.None)]
    [InlineData("1\u00A0234", "1234", NumericInputProblem.None)]
    [InlineData("-1,234.50", "-1234.50", NumericInputProblem.None)]
    [InlineData("\u22125", "-5", NumericInputProblem.None)]
    [InlineData("1234,567", "1234,567", NumericInputProblem.MisplacedGrouping)]
    [InlineData("1,,234", "1,,234", NumericInputProblem.MisplacedGrouping)]
    [InlineData(",5", ",5", NumericInputProblem.MisplacedGrouping)]
    [InlineData("1,234,56", "1,234,56", NumericInputProblem.MisplacedGrouping)]
    [InlineData("12,34", "12,34", NumericInputProblem.AmbiguousSeparator)]
    [InlineData("1,5000", "1,5000", NumericInputProblem.AmbiguousSeparator)]
    [InlineData("1.234,567", "1.234,567", NumericInputProblem.MisplacedGrouping)]
    [InlineData("1,234.5,6", "1,234.5,6", NumericInputProblem.AmbiguousSeparator)]
    [InlineData("12,34.5.6", "12,34.5.6", NumericInputProblem.UnknownCharacter)]
    [InlineData("-", "-", NumericInputProblem.Empty)]
    [InlineData(null, "", NumericInputProblem.Empty)]
    public void GroupingIsValidatedNotStripped(string? typed, string expectedText, NumericInputProblem expectedProblem)
    {
        var result = NumeralText.TryNormalizeNumericInput(typed);

        Assert.Equal(expectedProblem, result.Problem);
        Assert.Equal(expectedText, result.Normalized);
    }

    /// <summary>
    /// A refusal carries a message <b>key</b>, not prose, so the control translates it. The keys
    /// are the catalogue's: every problem except <see cref="NumericInputProblem.None"/> has one,
    /// they are distinct, and each looks like a key rather than a sentence.
    /// </summary>
    [Fact]
    public void EveryProblemCarriesItsOwnMessageKey()
    {
        Assert.Equal(string.Empty, NumeralText.Explain(NumericInputProblem.None));
        Assert.Equal("numeric.empty", NumeralText.Explain(NumericInputProblem.Empty));
        Assert.Equal("numeric.misplaced_grouping", NumeralText.Explain(NumericInputProblem.MisplacedGrouping));
        Assert.Equal("numeric.ambiguous_separator", NumeralText.Explain(NumericInputProblem.AmbiguousSeparator));
        Assert.Equal("numeric.unknown_character", NumeralText.Explain(NumericInputProblem.UnknownCharacter));

        var keys = Enum.GetValues<NumericInputProblem>()
            .Where(problem => problem != NumericInputProblem.None)
            .Select(NumeralText.Explain)
            .ToArray();

        Assert.Equal(keys.Length, keys.Distinct(StringComparer.Ordinal).Count());
        Assert.All(keys, key => Assert.Contains(key, NumericInputKeys.All));
        Assert.All(NumericInputKeys.All, key => Assert.Matches("^numeric\\.[a-z_]+$", key));
    }

    [Fact]
    public void TheTwoEasternDigitSetsNormaliseToTheSameValue()
    {
        Assert.True(NumeralText.TryParseDecimal("١٢٣٫٤٥", out var arabicIndic));
        Assert.True(NumeralText.TryParseDecimal("۱۲۳٫۴۵", out var extendedArabicIndic));

        Assert.Equal(arabicIndic, extendedArabicIndic);
        Assert.Equal(123.45m, arabicIndic);
        Assert.Equal(123.45m, extendedArabicIndic);
        Assert.Equal(123.45m.GetHashCode(), arabicIndic.GetHashCode());
    }

    [Fact]
    public void EasternDigitsAreDetectedAndMappedBothWays()
    {
        Assert.True(NumeralText.ContainsEasternDigits("١٢٣"));
        Assert.True(NumeralText.ContainsEasternDigits("۱۲۳"));
        Assert.False(NumeralText.ContainsEasternDigits("123"));

        Assert.Equal("۱۲۳", NumeralText.ToSystemDigits("123", NumeralSystem.ExtendedArabicIndic));
        Assert.Equal("١٢٣", NumeralText.ToSystemDigits("123", NumeralSystem.ArabicIndic));
        Assert.Equal("123", NumeralText.ToSystemDigits("123", NumeralSystem.Latin));
        Assert.Equal("123", NumeralText.ToLatinDigits("۱۲۳"));
        Assert.Equal("123", NumeralText.ToLatinDigits("١٢٣"));
        // The minus sign a Persian keyboard emits and the Arabic thousands separator.
        Assert.Equal("-1,234", NumeralText.ToLatinDigits("\u2212۱٬۲۳۴"));
    }

    [AvaloniaFact]
    public void EasternDigitsRoundTripThroughTheDecimalInputToTheSameDecimal()
    {
        var arabicIndic = new SpotDecimalInput { Scale = 2 };
        var extended = new SpotDecimalInput { Scale = 2 };
        var window = new Window { Width = 320, Height = 120, Content = new StackPanel { Children = { arabicIndic, extended } } };
        try
        {
            window.Show();
            Dispatcher.UIThread.RunJobs();

            arabicIndic.Text = "١٢٣٫٤٥";
            extended.Text = "۱۲۳٫۴۵";

            Assert.True(arabicIndic.Commit());
            Assert.True(extended.Commit());
            Assert.Null(arabicIndic.ErrorText);
            Assert.Null(extended.ErrorText);
            Assert.Equal(123.45m, arabicIndic.Value);
            Assert.Equal(arabicIndic.Value, extended.Value);
            Assert.Equal("123.45", arabicIndic.DisplayText(arabicIndic.Value));
        }
        finally
        {
            window.Close();
        }
    }

    [Theory]
    [InlineData(NumeralSystem.Latin, "1,234.50")]
    [InlineData(NumeralSystem.ArabicIndic, "١٬٢٣٤٫٥٠")]
    [InlineData(NumeralSystem.ExtendedArabicIndic, "۱٬۲۳۴٫۵۰")]
    public void MoneyIsDisplayedInTheConfiguredDigitSet(NumeralSystem system, string expected)
    {
        var format = NumeralFormat.For(system);

        Assert.Equal(expected, format.Money(Amount));
    }

    [Fact]
    public void MoneyFormattingKeepsTheSignAndNeverRoundsAValueItWasNotAskedTo()
    {
        var latin = NumeralFormat.For(NumeralSystem.Latin);

        Assert.Equal("-1,234.50", latin.Money(-Amount));
        Assert.Equal("0.00", latin.Money(0m));
        Assert.Equal("1,234.5678", latin.Money(1234.5678m));
        Assert.Equal("999.00", latin.Money(999m));
        Assert.Equal("1,000,000.00", latin.Money(1_000_000m));

        var extended = NumeralFormat.For(NumeralSystem.ExtendedArabicIndic);
        Assert.Equal("۱٬۲۳۴٫۵۶۷۸", extended.Money(1234.5678m));
    }

    [Fact]
    public void TheSettingDefaultsToLatinAndRoundTripsThroughStorage()
    {
        Assert.Equal(NumeralSystem.Latin, NumeralSettings.Default.System);
        Assert.Equal(NumeralSettings.Default, NumeralSettings.FromStorageValue(null));
        Assert.Equal(NumeralSettings.Default, NumeralSettings.FromStorageValue(string.Empty));
        Assert.Equal(NumeralSettings.Default, NumeralSettings.FromStorageValue("Klingon"));

        foreach (var system in Enum.GetValues<NumeralSystem>())
        {
            var settings = new NumeralSettings(system);
            Assert.Equal(settings, NumeralSettings.FromStorageValue(settings.ToStorageValue()));
            Assert.Equal(NumeralFormat.For(system), settings.Format);
        }
    }

    /// <summary>
    /// The digit and money paths must stay on <see cref="decimal"/>: a source scan is the only
    /// way a test can hold that line, because a float that never loses a digit looks correct.
    /// </summary>
    [Fact]
    public void TheNumeralAndBidiPrimitivesContainNoFloatingPoint()
    {
        var theme = Path.Combine(TestResources.RepositoryRoot, "clients", "avalonia", "src", "Spotledger.Client.Theme");

        foreach (var file in new[] { "NumeralSystem.cs", "BidiText.cs", "HighContrastDetector.cs", "ThemeVariants.cs" })
        {
            var source = File.ReadAllText(Path.Combine(theme, file));
            Assert.DoesNotContain("float ", source, StringComparison.Ordinal);
            Assert.DoesNotContain("double ", source, StringComparison.Ordinal);
            Assert.DoesNotContain("Math.Round(", source, StringComparison.Ordinal);
        }
    }

    [Fact]
    public void ParsingRejectsANonNumberInsteadOfGuessing()
    {
        Assert.False(NumeralText.TryParseDecimal("بارہ", out _));
        Assert.False(NumeralText.TryParseDecimal("12,34.5.6", out _));
        Assert.False(NumeralText.TryParseDecimal("12,34", out _));
        Assert.True(NumeralText.TryParseDecimal("12,34,567.50", out var grouped));
        Assert.Equal(1234567.50m, grouped);
    }

    [Fact]
    public void TheCultureNeverChangesTheResult()
    {
        var original = CultureInfo.CurrentCulture;
        try
        {
            // The European spelling is refused exactly as it is under invariant: the document's
            // format wins, and a separator that could be read two ways is never guessed.
            var invariant = NumeralText.TryNormalizeNumericInput("1.234,50");
            Assert.Equal(NumericInputProblem.AmbiguousSeparator, invariant.Problem);
            Assert.False(NumeralText.TryParseDecimal("1.234,50", out _));
            Assert.Equal("1,234.50", NumeralFormat.For(NumeralSystem.Latin).Money(Amount));

            // A German culture must not reinterpret the separators either.
            CultureInfo.CurrentCulture = CultureInfo.GetCultureInfo("de-DE");

            var german = NumeralText.TryNormalizeNumericInput("1.234,50");
            Assert.Equal(invariant, german);
            Assert.False(NumeralText.TryParseDecimal("1.234,50", out _));
            Assert.True(NumeralText.TryParseDecimal("1,234.50", out var parsed));
            Assert.Equal(1234.50m, parsed);
            Assert.Equal("1,234.50", NumeralFormat.For(NumeralSystem.Latin).Money(Amount));
        }
        finally
        {
            CultureInfo.CurrentCulture = original;
        }
    }

    /// <summary>
    /// Formatting and parsing are inverses in every digit set, for Western and for South Asian
    /// grouping. Random values rather than a fixed list: the grouping code has an off-by-one for
    /// every group count, and a fixed list only ever exercises the ones someone thought of.
    /// </summary>
    [Fact]
    public void MoneyFormattingAndParsingRoundTripInBothGroupingStylesAndAllDigitSets()
    {
        var random = new Random(1991);

        for (var iteration = 0; iteration < 200; iteration++)
        {
            var units = random.NextInt64(0, 1_000_000_000);
            var value = (decimal)units + (random.Next(0, 100) / 100m);

            foreach (var system in Enum.GetValues<NumeralSystem>())
            {
                var format = NumeralFormat.For(system);

                var western = format.Money(value);
                Assert.True(NumeralText.TryParseDecimal(western, out var westernValue), western);
                Assert.Equal(value, westernValue);

                var southAsian = SouthAsianGrouping(value, format);
                Assert.True(NumeralText.TryParseDecimal(southAsian, out var southAsianValue), southAsian);
                Assert.Equal(value, southAsianValue);
            }
        }
    }

    /// <summary>Two decimal places, grouped the South Asian way, in the digit set's characters.</summary>
    private static string SouthAsianGrouping(decimal value, NumeralFormat format)
    {
        var text = value.ToString("F2", CultureInfo.InvariantCulture);
        var point = text.IndexOf('.', StringComparison.Ordinal);
        var integer = text[..point];
        var fraction = text[(point + 1)..];

        var builder = new StringBuilder();
        if (integer.Length <= 3)
        {
            builder.Append(integer);
        }
        else
        {
            var rest = integer[..^3];
            var first = rest.Length % 2 == 0 ? 2 : 1;
            builder.Append(rest, 0, first);
            for (var index = first; index < rest.Length; index += 2)
            {
                builder.Append(',').Append(rest, index, 2);
            }

            builder.Append(',').Append(integer, integer.Length - 3, 3);
        }

        builder.Append('.').Append(fraction);
        var mapped = NumeralText.ToSystemDigits(builder.ToString(), format.System);
        return format.System == NumeralSystem.Latin
            ? mapped
            : mapped.Replace(",", format.GroupSeparator, StringComparison.Ordinal)
                .Replace(".", format.DecimalSeparator, StringComparison.Ordinal);
    }
}
