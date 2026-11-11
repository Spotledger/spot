using System.Reflection;
using Avalonia.Headless.XUnit;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Recorded security findings (host trust hardening): a revocation list reaches what is installed, its
/// generation must be strictly higher, and nothing is decided by the date a publisher wrote into its
/// own manifest. Tier B components run in this process with its full authority: disabling stops
/// further use, it does not make a component that already ran safe.
/// </summary>
public sealed class RevocationHardeningTests
{
    private static readonly string[] UnloadedAcme = { "acme.rating@1.0.0" };

    [Fact]
    public void RevokingTheKeyOfAnInstalledComponentDisablesAndUnloadsIt()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        var loaded = harness.Service.Install(package.Root, HardeningHarness.Now).Component!;

        harness.Trust.Accept(harness.Trust.Revoke(
            generation: 2,
            keys: new[] { new RevokedKeyEntry(harness.Trust.PublisherKeyId, 1_750_000_000, false, "withdrawn key") }));

        var outcome = Assert.Single(harness.Service.RevocationOutcomes);
        Assert.Equal("acme.rating", outcome.ComponentId);
        Assert.Equal(ComponentRefusalCodes.KeyRevoked, outcome.Refusal.Code);
        Assert.True(outcome.Unloaded);
        Assert.False(outcome.RestartRequired);
        Assert.False(harness.Service.RestartRequired);
        Assert.True(loaded.IsDisabled);
        Assert.True(loaded.IsUnloaded);
        Assert.Equal(0, harness.Service.Catalogue.Count);
        Assert.Equal(UnloadedAcme, harness.Log.UnloadedIds);
    }

    [Theory]
    [InlineData("key", ComponentRefusalCodes.KeyRevoked)]
    [InlineData("compromised", ComponentRefusalCodes.KeyCompromised)]
    [InlineData("publisher", ComponentRefusalCodes.PublisherRevoked)]
    [InlineData("package", ComponentRefusalCodes.PackageRevoked)]
    public void EveryKindOfRevocationDisablesWhatIsInstalled(string kind, string expectedCode)
    {
        using var harness = new HardeningHarness();
        var (package, manifest, _) = harness.Build();
        using var cleanup = package;
        harness.Service.Install(package.Root, HardeningHarness.Now);

        var list = kind switch
        {
            "key" => harness.Trust.Revoke(2, keys: new[] { new RevokedKeyEntry(harness.Trust.PublisherKeyId, 1, false, "r") }),
            "compromised" => harness.Trust.Revoke(2, keys: new[] { new RevokedKeyEntry(harness.Trust.PublisherKeyId, 1, true, "r") }),
            "publisher" => harness.Trust.Revoke(2, publishers: new[] { new RevokedPublisherEntry("acme", 1, "r") }),
            _ => harness.Trust.Revoke(2, packages: new[] { new RevokedPackageEntry(manifest.SignedBytesSha256(), "r") }),
        };
        harness.Trust.Accept(list);

        Assert.Equal(expectedCode, Assert.Single(harness.Service.RevocationOutcomes).Refusal.Code);
        Assert.Equal(0, harness.Service.Catalogue.Count);
    }

    [Fact]
    public void ARevocationOfSomethingElseLeavesTheComponentInstalled()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        harness.Service.Install(package.Root, HardeningHarness.Now);

        harness.Trust.Accept(harness.Trust.Revoke(
            2,
            keys: new[] { new RevokedKeyEntry(harness.Trust.OutsiderKeyId, 1, true, "unrelated") },
            publishers: new[] { new RevokedPublisherEntry("someone-else", 1, "unrelated") }));

        Assert.Empty(harness.Service.RevocationOutcomes);
        Assert.Empty(harness.Service.RecheckInstalled());
        Assert.Equal(1, harness.Service.Catalogue.Count);
    }

    [Fact]
    public void ADisabledComponentCreatesNoNewInstanceAndSaysWhyInPlainText()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        var loaded = harness.Service.Install(package.Root, HardeningHarness.Now).Component!;
        harness.Trust.Accept(harness.Trust.Revoke(
            2, publishers: new[] { new RevokedPublisherEntry("acme", 1, "publisher withdrawn") }));

        var created = loaded.TryCreate(TestDescriptors.Context(), null, out var instance, out var refusal);

        Assert.False(created);
        Assert.Null(instance);
        Assert.Equal(ComponentRefusalCodes.RevokedDisabled, refusal!.Code);
        AssertPlain(refusal.Message);
        Assert.Throws<InvalidOperationException>(() => loaded.Create(TestDescriptors.Context()));
    }

    [AvaloniaFact]
    public void ACodeThatCannotBeTornDownMarksRestartRequiredAndStaysRefused()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        var loaded = harness.Service.Install(package.Root, HardeningHarness.Now).Component!;

        // A live instance whose teardown throws: the context cannot be released cleanly.
        var stub = new StubComponent(TestDescriptors.Stub(), TestDescriptors.Context(), throwOnUpdate: false)
        {
            ThrowOnDispose = true,
        };
        var instance = (ComponentInstance)Activator.CreateInstance(
            typeof(ComponentInstance),
            BindingFlags.Instance | BindingFlags.NonPublic,
            binder: null,
            args: new object[] { loaded, stub, ComponentCallbacks.None },
            culture: null)!;
        var instances = (List<ComponentInstance>)typeof(LoadedComponent)
            .GetField("_instances", BindingFlags.Instance | BindingFlags.NonPublic)!
            .GetValue(loaded)!;
        instances.Add(instance);

        harness.Trust.Accept(harness.Trust.Revoke(
            2, publishers: new[] { new RevokedPublisherEntry("acme", 1, "publisher withdrawn") }));

        var outcome = Assert.Single(harness.Service.RevocationOutcomes);
        Assert.False(outcome.Unloaded);
        Assert.True(outcome.RestartRequired);
        Assert.True(harness.Service.RestartRequired);
        Assert.False(loaded.IsUnloaded);
        Assert.True(loaded.IsDisabled);
        Assert.False(loaded.TryCreate(TestDescriptors.Context(), null, out _, out var refusal));
        Assert.Equal(ComponentRefusalCodes.RevokedDisabled, refusal!.Code);
    }

    [Fact]
    public void ARevocationIsRememberedAcrossARestart()
    {
        var path = Path.Combine(Path.GetTempPath(), "spot-state-" + Guid.NewGuid().ToString("N") + ".json");
        try
        {
            using var package = new TempPackage();
            using (var first = new HardeningHarness(new FileComponentStateStore(path)))
            {
                PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(first.Trust));
                Assert.True(first.Service.Install(package.Root, HardeningHarness.Now).IsLoaded);
                first.Trust.Accept(first.Trust.Revoke(
                    2, publishers: new[] { new RevokedPublisherEntry("acme", 1, "publisher withdrawn") }));
                Assert.Single(first.Service.RevocationOutcomes);
            }

            // A new process: a fresh trust store that has not been given the list yet.
            using var second = new HardeningHarness(new FileComponentStateStore(path));
            var result = second.Service.Install(package.Root, HardeningHarness.Now);

            Assert.False(result.IsLoaded);
            Assert.Equal(ComponentRefusalCodes.PackageRevoked, result.Refusal!.Code);
            Assert.Equal(0, second.Service.Catalogue.Count);
        }
        finally
        {
            File.Delete(path);
        }
    }

    [Fact]
    public void ARevocationListWithALowerEqualOrHigherGenerationIsHandledAsTheRuleSays()
    {
        var harness = new HardeningHarness();
        harness.Trust.Accept(harness.Trust.Revoke(5));

        var lower = harness.Trust.Store.AcceptRevocationList(harness.Trust.Revoke(4), HardeningHarness.Now);
        Assert.Equal(ComponentRefusalCodes.StaleRevocationList, lower!.Code);

        // Equal generation, validly signed, different contents: refused all the same.
        var different = harness.Trust.Revoke(
            5, publishers: new[] { new RevokedPublisherEntry("acme", 1, "swapped contents") });
        var equal = harness.Trust.Store.AcceptRevocationList(different, HardeningHarness.Now);
        Assert.Equal(ComponentRefusalCodes.GenerationNotNewer, equal!.Code);
        Assert.Empty(harness.Trust.Store.Revocations!.Publishers);
        Assert.Equal(5UL, harness.Trust.Store.HeldRevocationGeneration);
        AssertPlain(equal.Message);

        var higher = harness.Trust.Store.AcceptRevocationList(
            harness.Trust.Revoke(6, publishers: new[] { new RevokedPublisherEntry("acme", 1, "ok") }),
            HardeningHarness.Now);
        Assert.Null(higher);
        Assert.Equal(6UL, harness.Trust.Store.HeldRevocationGeneration);
    }

    [Fact]
    public void TheFirstListMayBeGenerationZeroButNotTwice()
    {
        var harness = new HardeningHarness();

        Assert.Null(harness.Trust.Store.AcceptRevocationList(harness.Trust.Revoke(0), HardeningHarness.Now));
        Assert.Equal(
            ComponentRefusalCodes.GenerationNotNewer,
            harness.Trust.Store.AcceptRevocationList(harness.Trust.Revoke(0), HardeningHarness.Now)!.Code);
    }

    [Fact]
    public void ARevokedKeyIsRefusedWhateverDateThePackageClaims()
    {
        // The key is revoked "from" 1_750_000_000. A package that claims it was signed long before
        // is refused all the same: the claim is the publisher's own.
        foreach (var claimedAt in new long[] { 1, 1_600_000_000, 1_749_999_999, 1_760_000_000, 4_000_000_000 })
        {
            using var harness = new HardeningHarness();
            var (package, _, _) = harness.Build(spec => spec.SignedAt = claimedAt);
            using var cleanup = package;
            harness.Trust.Accept(harness.Trust.Revoke(
                2, keys: new[] { new RevokedKeyEntry(harness.Trust.PublisherKeyId, 1_750_000_000, false, "withdrawn key") }));

            var result = harness.Service.Install(package.Root, HardeningHarness.Now);

            Assert.False(result.IsLoaded);
            Assert.Equal(ComponentRefusalCodes.KeyRevoked, result.Refusal!.Code);
        }
    }

    [Fact]
    public void TheClaimedSigningDateDecidesNothingAndIsLabelledAsClaimed()
    {
        using var harness = new HardeningHarness();

        // Claimed long before the certificate's window: this used to be refused for the claim alone.
        var (package, _, _) = harness.Build(spec => spec.SignedAt = 5);
        using var cleanup = package;
        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.True(result.IsLoaded, result.Refusal?.ToString());
        Assert.Equal(5, result.Component!.Verified.SignedAt);
        Assert.Contains("claimed by the publisher", result.Component.ToString(), StringComparison.Ordinal);
        Assert.DoesNotContain(
            typeof(ComponentRefusalCodes).GetFields(BindingFlags.Public | BindingFlags.Static),
            f => f.Name == "SignedOutsideWindow");
    }

    private static void AssertPlain(string message)
    {
        Assert.DoesNotContain("   at ", message, StringComparison.Ordinal);
        Assert.DoesNotContain("Exception", message, StringComparison.Ordinal);
        Assert.DoesNotContain("\n", message, StringComparison.Ordinal);
    }
}
