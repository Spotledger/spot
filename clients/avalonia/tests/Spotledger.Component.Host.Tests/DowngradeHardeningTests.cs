using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Recorded security finding (downgrade) and the install memory behind it: the host remembers, per component id, the
/// highest version it installed and the highest revocation generation it installed under. The sample
/// component is version 1.0.0, so "an older version" is a memory of a higher one a previous run left.
/// </summary>
public sealed class DowngradeHardeningTests
{
    [Fact]
    public void AnOlderVersionThanTheHighestEverInstalledIsRefusedAndNothingLoads()
    {
        using var harness = new HardeningHarness();
        harness.Seed("acme.rating", highestVersion: "2.0.0");
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.False(result.IsLoaded);
        Assert.Equal(ComponentRefusalCodes.DowngradeRefused, result.Refusal!.Code);
        Assert.Contains("1.0.0", result.Refusal.Message, StringComparison.Ordinal);
        Assert.Contains("2.0.0", result.Refusal.Message, StringComparison.Ordinal);
        Assert.Contains("Revoke the newer version", result.Refusal.Message, StringComparison.Ordinal);
        Assert.Equal(0, harness.Service.Catalogue.Count);
        Assert.Empty(harness.Log.LoadedIds);
    }

    [Fact]
    public void TheSameRuleHoldsForAnUpgradeCall()
    {
        using var harness = new HardeningHarness();
        harness.Seed("acme.rating", highestVersion: "2.0.0");
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        var result = harness.Service.Upgrade(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.DowngradeRefused, result.Refusal!.Code);
    }

    [Fact]
    public void AnExplicitAllowanceWithAnActorInstallsAndIsRecordedInTheLog()
    {
        using var harness = new HardeningHarness();
        harness.Seed("acme.rating", highestVersion: "2.0.0");
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        var result = harness.Service.Install(
            package.Root, HardeningHarness.Now, new InstallOptions(AllowDowngrade: true, Actor: "alice"));

        Assert.True(result.IsLoaded, result.Refusal?.ToString());
        harness.State.TryLoad(out var state, out _);
        var entry = Assert.Single(state.Log);
        Assert.True(entry.DowngradeAllowed);
        Assert.Equal("alice", entry.Actor);
        Assert.Equal("2.0.0", entry.PreviousHighest);
        Assert.Equal("1.0.0", entry.Version);
        Assert.Equal(HardeningHarness.Now, entry.At);

        // The highest version stays the highest: the downgrade does not lower the bar.
        Assert.Equal("2.0.0", state.HighestVersion["acme.rating"]);
    }

    [Fact]
    public void AnAllowanceWithoutAnActorIsRefusedBecauseNobodyWouldBeAccountableForIt()
    {
        using var harness = new HardeningHarness();
        harness.Seed("acme.rating", highestVersion: "2.0.0");
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        var result = harness.Service.Install(package.Root, HardeningHarness.Now, new InstallOptions(AllowDowngrade: true));

        Assert.Equal(ComponentRefusalCodes.DowngradeRefused, result.Refusal!.Code);
    }

    [Fact]
    public void AnEqualOrHigherVersionInstallsAndTheHighestIsRemembered()
    {
        using var harness = new HardeningHarness();
        harness.Seed("acme.rating", highestVersion: "1.0.0");
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        Assert.True(harness.Service.Install(package.Root, HardeningHarness.Now).IsLoaded);

        harness.State.TryLoad(out var state, out _);
        Assert.Equal("1.0.0", state.HighestVersion["acme.rating"]);
        Assert.False(Assert.Single(state.Log).DowngradeAllowed);
    }

    [Fact]
    public void TheHighestRevocationGenerationInstalledUnderIsRemembered()
    {
        using var harness = new HardeningHarness();
        harness.Trust.Accept(harness.Trust.Revoke(7));
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        Assert.True(harness.Service.Install(package.Root, HardeningHarness.Now).IsLoaded);

        harness.State.TryLoad(out var state, out _);
        Assert.Equal(7UL, state.HighestGeneration["acme.rating"]);
    }

    [Fact]
    public void AStoreThatNowHoldsAnOlderGenerationThanTheOneInstalledUnderIsRefused()
    {
        using var harness = new HardeningHarness();
        harness.Seed("acme.rating", highestGeneration: 9);
        harness.Trust.Accept(harness.Trust.Revoke(3));
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.GenerationRegressed, result.Refusal!.Code);
        Assert.Equal(0, harness.Service.Catalogue.Count);
    }

    [Fact]
    public void AStateFileThatCannotBeReadRefusesInstallsInsteadOfDroppingTheRules()
    {
        var path = Path.Combine(Path.GetTempPath(), "spot-state-" + Guid.NewGuid().ToString("N") + ".json");
        try
        {
            File.WriteAllText(path, "{ this is not json");
            using var harness = new HardeningHarness(new FileComponentStateStore(path));
            var (package, _, _) = harness.Build();
            using var cleanup = package;

            var result = harness.Service.Install(package.Root, HardeningHarness.Now);

            Assert.Equal(ComponentRefusalCodes.StateUnreadable, result.Refusal!.Code);
            Assert.DoesNotContain("   at ", result.Refusal.Message, StringComparison.Ordinal);
            Assert.Equal(0, harness.Service.Catalogue.Count);
        }
        finally
        {
            File.Delete(path);
        }
    }

    [Fact]
    public void AStateThatCannotBeWrittenRefusesTheInstallAndLeavesNothingInstalled()
    {
        using var harness = new HardeningHarness(new UnwritableStateStore());
        var (package, _, _) = harness.Build();
        using var cleanup = package;

        var result = harness.Service.Install(package.Root, HardeningHarness.Now);

        Assert.Equal(ComponentRefusalCodes.StateUnwritable, result.Refusal!.Code);
        Assert.Equal(0, harness.Service.Catalogue.Count);
    }
}
