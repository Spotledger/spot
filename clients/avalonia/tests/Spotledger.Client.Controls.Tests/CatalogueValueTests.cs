using System.Globalization;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class CatalogueValueTests
{
    [AvaloniaFact]
    public void PercentPreservesOneHundredPreciseDecimalValues()
    {
        var random = new Random(5701);
        var input = new SpotPercentInput { Scale = 12 };
        for (var i = 0; i < 100; i++)
        {
            var value = new decimal(random.Next(), random.Next(), random.Next(100), i % 2 == 0, 12);
            input.Text = value.ToString(CultureInfo.InvariantCulture);
            Assert.True(input.Commit());
            Assert.Equal(value, input.Value);
        }
    }

    [AvaloniaTheory]
    [InlineData("2026-10-15T14:30:00+05:00", true)]
    [InlineData("2026-10-15T14:30:00Z", true)]
    [InlineData("2026-10-15 14:30:00-03:30", true)]
    [InlineData("2026-10-15T14:30:00", false)]
    [InlineData("not a date", false)]
    public void DateTimeRequiresAndPreservesExplicitOffset(string text, bool valid)
    {
        var input = new SpotDateTimeInput { Text = text };
        Assert.Equal(valid, input.Commit());
        if (valid)
        {
            var expected = DateTimeOffset.Parse(text, CultureInfo.InvariantCulture);
            Assert.Equal(expected.Ticks, input.Value!.Value.Ticks);
            Assert.Equal(expected.Offset, input.Value.Value.Offset);
            Assert.Contains(expected.ToString("zzz", CultureInfo.InvariantCulture), input.Text);
        }
    }

    [AvaloniaTheory]
    [InlineData("14:30", true)]
    [InlineData("2:30 PM", true)]
    [InlineData("23:59:59.1234567", true)]
    [InlineData("25:00", false)]
    public void TimeParsesPastedForms(string text, bool valid)
    {
        var input = new SpotTimeInput { Text = text };
        Assert.Equal(valid, input.Commit());
        if (valid) Assert.Equal(TimeOnly.Parse(text, CultureInfo.InvariantCulture), input.Value);
    }
}
