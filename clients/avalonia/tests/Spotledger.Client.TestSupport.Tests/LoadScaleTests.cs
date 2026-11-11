using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

public sealed class LoadScaleTests
{
    [Fact]
    public void AnOverrideWins()
    {
        Assert.Equal(2.5, LoadScale.From(40, 4, "2.5"));
        Assert.Equal(0.5, LoadScale.From(null, 4, "0.5"));
    }

    [Theory]
    [InlineData(0.0, 8)]
    [InlineData(3.9, 4)]
    [InlineData(4.0, 4)]
    public void LoadBelowTheCoreCountGivesOne(double load, int processors) =>
        Assert.Equal(1, LoadScale.From(load, processors, null));

    [Fact]
    public void LoadOfTwiceTheCoresGivesTwo() => Assert.Equal(2, LoadScale.From(8, 4, null));

    [Fact]
    public void TheScaleIsClampedAtFour()
    {
        Assert.Equal(4, LoadScale.From(400, 4, null));
        Assert.Equal(4, LoadScale.From(16, 4, null));
    }

    [Theory]
    [InlineData("fast")]
    [InlineData("0")]
    [InlineData("-2")]
    [InlineData("NaN")]
    [InlineData("Infinity")]
    public void GarbageInTheVariableIsRefusedWithAMessage(string value)
    {
        var error = Assert.Throws<ArgumentException>(() => LoadScale.From(1, 4, value));
        Assert.Contains(LoadScale.OverrideVariable, error.Message, StringComparison.Ordinal);
        Assert.Contains(value, error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AMissingFileGivesOne()
    {
        var path = Path.Combine(Path.GetTempPath(), "no-such-loadavg-" + Guid.NewGuid().ToString("N"));
        Assert.Null(LoadScale.ReadLoadAverage(path));
        Assert.Equal(1, LoadScale.From(LoadScale.ReadLoadAverage(path), 4, null));
    }

    [Fact]
    public void TheFirstFieldOfALoadavgFileIsRead()
    {
        var path = Path.Combine(Path.GetTempPath(), "loadavg-" + Guid.NewGuid().ToString("N"));
        File.WriteAllText(path, "6.50 3.10 2.00 4/900 12345\n");
        try
        {
            Assert.Equal(6.5, LoadScale.ReadLoadAverage(path));
        }
        finally
        {
            File.Delete(path);
        }
    }
}
