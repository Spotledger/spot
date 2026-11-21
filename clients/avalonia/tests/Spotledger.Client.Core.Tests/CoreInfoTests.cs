using Spotledger.Client.Core;
using Xunit;

namespace Spotledger.Client.Core.Tests;

public sealed class CoreInfoTests
{
    [Fact]
    public void VersionIsDefined()
    {
        Assert.Equal("0.1.0", CoreInfo.Version);
    }
}
