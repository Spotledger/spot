using System.Text.Json;
using Spotledger.Component.Contract;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// The value codec: every field type keeps an exact canonical encoding, a null value stays null,
/// and the empty value is the type's zero shape. No float ever touches money or quantities.
/// </summary>
public sealed class ValueCodecTests
{
    [Fact]
    public void EveryFieldTypeHasACanonicalValue()
    {
        Assert.Equal(15, ComponentFieldTypes.All.Count);
        foreach (var type in ComponentFieldTypes.All)
        {
            var value = SampleValues.Canonical(type);
            Assert.Equal(type, value.Type);
            Assert.False(value.IsNull);
            Assert.Equal(value, SampleValues.Canonical(type));
        }
    }

    [Fact]
    public void BooleanRoundTrips()
    {
        Assert.True(ComponentValue.FromBoolean(true).TryGetBoolean(out var flag));
        Assert.True(flag);
        Assert.Equal("true", ComponentValue.FromBoolean(true).Text);
        Assert.Equal("false", ComponentValue.FromBoolean(false).Text);
    }

    [Fact]
    public void IntegerRoundTripsIncludingTheExtremes()
    {
        foreach (var number in new[] { 0L, 1L, -7L, long.MaxValue, long.MinValue })
        {
            var value = ComponentValue.FromInteger(number);
            Assert.True(value.TryGetInteger(out var read));
            Assert.Equal(number, read);
        }
    }

    [Fact]
    public void DecimalKeepsItsExactScale()
    {
        var value = ComponentValue.FromDecimal(12.50m);

        Assert.Equal("12.50", value.Text);
        Assert.True(value.TryGetDecimal(out var read));
        Assert.Equal(12.50m, read);
        Assert.Equal(2, decimal.GetBits(read)[3] >> 16 & 0xFF);
    }

    [Fact]
    public void DateAndTimeUseTheContractEncodings()
    {
        var date = ComponentValue.FromDate(new DateOnly(2026, 12, 31));
        Assert.Equal("2026-12-31", date.Text);
        Assert.True(date.TryGetDate(out var readDate));
        Assert.Equal(new DateOnly(2026, 12, 31), readDate);

        var time = ComponentValue.FromTime(new TimeOnly(9, 5, 4));
        Assert.Equal("09:05:04", time.Text);
        Assert.True(time.TryGetTime(out var readTime));
        Assert.Equal(new TimeOnly(9, 5, 4), readTime);
    }

    [Fact]
    public void InstantKeepsItsOffset()
    {
        var instant = new DateTimeOffset(2026, 6, 1, 8, 30, 0, TimeSpan.FromHours(5));
        var value = ComponentValue.FromInstant(instant);

        Assert.True(value.TryGetInstant(out var read));
        Assert.Equal(instant, read);
        Assert.Equal(instant.Offset, read.Offset);
    }

    [Fact]
    public void LinkCarriesIdAndDisplayText()
    {
        var value = ComponentValue.FromLink("ITEM-1", "Widget");

        Assert.Equal("ITEM-1", value.LinkId);
        Assert.Equal("Widget", value.LinkDisplayText);
        Assert.Null(ComponentValue.FromLink("ITEM-1").LinkDisplayText);
    }

    [Fact]
    public void JsonIsKeptAsText()
    {
        var value = ComponentValue.FromJson("{\"a\":1,\"b\":[2,3]}");

        Assert.Equal(ComponentFieldType.Json, value.Type);
        Assert.Equal(1, JsonDocument.Parse(value.Text!).RootElement.GetProperty("a").GetInt32());
    }

    [Fact]
    public void BytesAreBase64()
    {
        var value = ComponentValue.FromBytes(new byte[] { 0, 1, 255 });

        Assert.Equal("AAH/", value.Text);
    }

    [Fact]
    public void NullAndEmptyAreDistinct()
    {
        foreach (var type in ComponentFieldTypes.All)
        {
            var nullValue = ComponentValue.Null(type);
            var empty = ComponentValue.Empty(type);

            Assert.True(nullValue.IsNull);
            Assert.True(nullValue.IsEmpty);
            Assert.False(empty.IsNull);
            Assert.Equal(type, empty.Type);
        }

        Assert.True(ComponentValue.Empty(ComponentFieldType.Text).IsEmpty);
        Assert.True(ComponentValue.Empty(ComponentFieldType.IntegerNumber).TryGetInteger(out var zero));
        Assert.Equal(0, zero);
    }

    [Fact]
    public void TypedAccessorsRefuseTheWrongType()
    {
        var text = ComponentValue.FromText("5");

        Assert.False(text.TryGetInteger(out _));
        Assert.False(text.TryGetDecimal(out _));
        Assert.False(text.TryGetBoolean(out _));
        Assert.False(text.TryGetDate(out _));
    }

    [Fact]
    public void WireNamesRoundTrip()
    {
        foreach (var type in ComponentFieldTypes.All)
        {
            var name = type.ToWireName();
            Assert.Equal(type, ComponentFieldTypes.TryParse(name));
        }

        Assert.Null(ComponentFieldTypes.TryParse("money"));
        Assert.Throws<ArgumentOutOfRangeException>(() => ((ComponentFieldType)999).ToWireName());
    }
}
