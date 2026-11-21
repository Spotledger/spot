using Acme.RatingComponent;
using Spotledger.Component.Contract;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// The contract's versioning rules, as behaviour: a major bump is a break the host refuses, a
/// minor is additive and accepted, and a range that would straddle a major is never valid.
/// </summary>
public sealed class ContractVersioningTests
{
    [Fact]
    public void HostAcceptsTheCurrentMajorRange()
    {
        Assert.True(ComponentContract.TryAccept(ContractVersionRange.CurrentMajor, out var code, out var message));
        Assert.Null(code);
        Assert.Null(message);
    }

    [Fact]
    public void HostRefusesAComponentThatNeedsANewerContract()
    {
        var nextMajor = new ContractVersionRange(new ContractVersion(2, 0), new ContractVersion(3, 0));

        Assert.False(ComponentContract.TryAccept(nextMajor, out var code, out var message));
        Assert.Equal("component.contract.too_new", code);
        Assert.Contains("2.0", message, StringComparison.Ordinal);
    }

    [Fact]
    public void HostRefusesAComponentBuiltForAnOlderMajor()
    {
        var oldMajor = new ContractVersionRange(new ContractVersion(0, 3), new ContractVersion(1, 0));

        Assert.False(ComponentContract.TryAccept(oldMajor, out var code, out _));
        Assert.Equal("component.contract.unsupported", code);
    }

    [Fact]
    public void MinorAdditionsKeepCompatibility()
    {
        // A component built for 1.0 keeps working on a host at 1.4: the range covers the major.
        var declared = ContractVersionRange.ForMajor(new ContractVersion(1, 0));
        Assert.True(declared.Contains(new ContractVersion(1, 4)));
        Assert.True(declared.Contains(new ContractVersion(1, 0)));
        Assert.False(declared.Contains(new ContractVersion(2, 0)));
    }

    [Fact]
    public void RangeNeverStraddlesAMajorVersion()
    {
        Assert.Throws<ArgumentException>(() => new ContractVersionRange(new ContractVersion(1, 0), new ContractVersion(3, 0)));
        Assert.False(ContractVersionRange.TryParse(">=1.0 <3.0", out _));
    }

    [Theory]
    [InlineData("1.0", 1, 0)]
    [InlineData("2.13", 2, 13)]
    public void ContractVersionParsesMajorDotMinor(string text, int major, int minor)
    {
        var version = ContractVersion.Parse(text);
        Assert.Equal(new ContractVersion(major, minor), version);
        Assert.Equal(text, version.ToString());
    }

    [Theory]
    [InlineData("1")]
    [InlineData("1.0.0")]
    [InlineData("v1.0")]
    [InlineData("")]
    public void ContractVersionRefusesAnythingElse(string text)
    {
        Assert.False(ContractVersion.TryParse(text, out _));
        Assert.Throws<FormatException>(() => ContractVersion.Parse(text));
    }

    [Fact]
    public void RangeRoundTripsThroughItsWireForm()
    {
        var range = ContractVersionRange.Parse(">=1.0 <2.0");
        Assert.Equal(new ContractVersion(1, 0), range.Minimum);
        Assert.Equal(new ContractVersion(2, 0), range.MaximumExclusive);
        Assert.Equal(">=1.0 <2.0", range.ToString());
        Assert.Equal(range, ContractVersionRange.Parse(range.ToString()));
    }

    [Fact]
    public void RegistryRefusesAnIncompatibleDescriptor()
    {
        var registry = new ComponentRegistry();
        var future = DescribeWithRange(ContractVersionRange.ForMajor(new ContractVersion(2, 0)));

        var result = registry.Register(future);

        Assert.False(result.Accepted);
        Assert.Equal("component.contract.too_new", result.Code);
        Assert.Empty(registry.Components);
    }

    [Fact]
    public void RegistryRefusesATakenId()
    {
        var registry = new ComponentRegistry();
        Assert.True(registry.Register(AcmeRatingDescriptor.Instance).Accepted);

        var duplicate = registry.Register(AcmeRatingDescriptor.Instance);

        Assert.False(duplicate.Accepted);
        Assert.Equal("component.id.duplicate", duplicate.Code);
        Assert.Single(registry.Components);
    }

    [Fact]
    public void RegistryRefusesAnUnsoundDescriptor()
    {
        var registry = new ComponentRegistry();
        var unsound = new ComponentDescriptor(
            "Not An Id",
            "Broken",
            new ComponentVersion(1, 0, 0),
            "acme",
            ContractVersionRange.CurrentMajor,
            Array.Empty<ComponentFieldType>(),
            "{ not json",
            new Uri("avares://Acme.RatingComponent/Themes/AcmeRating.axaml"),
            static context => new AcmeRatingComponent(context));

        var result = registry.Register(unsound);

        Assert.False(result.Accepted);
        Assert.Equal("component.descriptor.invalid", result.Code);
    }

    internal static ComponentDescriptor DescribeWithRange(ContractVersionRange range) => new(
        "acme.rating.future",
        "Future rating",
        new ComponentVersion(1, 0, 0),
        "acme",
        range,
        new[] { ComponentFieldType.IntegerNumber },
        AcmeRatingDescriptor.PropsSchemaJson,
        AcmeRatingDescriptor.ThemeSource,
        static context => new AcmeRatingComponent(context));
}
