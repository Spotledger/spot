using Acme.RatingComponent;
using Spotledger.Component.Contract;
using Spotledger.Component.TestKit;
using Xunit;

namespace Spotledger.Component.Tests;

/// <summary>
/// The sample package against the whole contract kit: every inherited fact is a contract rule, so
/// this class is the acceptance test the store's review pipeline would run on Acme's submission.
/// </summary>
public sealed class AcmeRatingContractTests : ComponentContractTests<AcmeRatingComponent>
{
    /// <inheritdoc />
    protected override ComponentDescriptor Descriptor => AcmeRatingDescriptor.Instance;

    [Fact]
    public void PackageDeclaresItsIdentity()
    {
        Assert.Equal("acme.rating", AcmeRatingDescriptor.Instance.Id);
        Assert.Equal("acme", AcmeRatingDescriptor.Instance.Publisher);
        Assert.Equal(new ComponentVersion(1, 0, 0), AcmeRatingDescriptor.Instance.Version);
        Assert.Equal(new[] { ComponentFieldType.IntegerNumber }, AcmeRatingDescriptor.Instance.SupportedFieldTypes);
        Assert.True(AcmeRatingDescriptor.Instance.ContractRange.Contains(ComponentContract.Version));
    }
}
