using Avalonia.Headless.XUnit;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Recorded security findings. A revocation that disabled a component must reach the install state, or the
/// host must say it did not: a failed read or save is reported (a stable code, never swallowed), it
/// sets <c>RestartRequired</c>, and new installs are refused until the disabled set has been saved.
/// Revoking a component that has run in this process also sets <c>RestartRequired</c>, because the
/// unload probe proves collectibility only, not that a component's threads stopped.
/// </summary>
public sealed class RevocationPersistenceTests
{
    private static readonly RevokedPublisherEntry[] AcmeRevoked = { new("acme", 1, "publisher withdrawn") };

    [Theory]
    [InlineData(StoreFault.SaveFails)]
    [InlineData(StoreFault.LoadFails)]
    [InlineData(StoreFault.SaveThrows)]
    [InlineData(StoreFault.LoadThrows)]
    public void ARevocationThatCannotBePersistedIsReportedAndBlocksInstalls(StoreFault fault)
    {
        var store = new FaultyStateStore();
        using var harness = new HardeningHarness(store);
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        harness.Service.Install(package.Root, HardeningHarness.Now);

        store.Fault = fault;
        harness.Trust.Accept(harness.Trust.Revoke(2, publishers: AcmeRevoked));

        var outcome = Assert.Single(harness.Service.RevocationOutcomes);
        Assert.NotNull(outcome.PersistenceFailure);
        Assert.Equal(ComponentRefusalCodes.RevocationNotPersisted, outcome.PersistenceFailure!.Code);
        Assert.DoesNotContain("secret", outcome.PersistenceFailure.Message, StringComparison.Ordinal);
        Assert.True(harness.Service.RestartRequired);
        Assert.False(harness.Service.RevocationsPersisted);
        Assert.Equal(ComponentRefusalCodes.RevocationNotPersisted, harness.Service.RevocationPersistenceFailure!.Code);
        Assert.Contains(harness.Log.Refusals, r => r.Code == ComponentRefusalCodes.RevocationNotPersisted);

        // A new install is refused while the disabled set is unsaved, whatever the package.
        store.Fault = StoreFault.None;
        var (other, _, _) = harness.Build(s => s.ComponentId = "acme.other");
        using var otherCleanup = other;
        var refused = harness.Service.Install(other.Root, HardeningHarness.Now);
        Assert.False(refused.IsLoaded);
        Assert.Equal(ComponentRefusalCodes.RevocationNotPersisted, refused.Refusal!.Code);
        Assert.DoesNotContain(other.Root, refused.Refusal.Message, StringComparison.Ordinal);

        var upgraded = harness.Service.Upgrade(other.Root, HardeningHarness.Now);
        Assert.Equal(ComponentRefusalCodes.RevocationNotPersisted, upgraded.Refusal!.Code);
    }

    [Fact]
    public void RetryingSavesTheDisabledSetAndInstallsWorkAgain()
    {
        var store = new FaultyStateStore();
        using var harness = new HardeningHarness(store);
        var (package, manifest, _) = harness.Build();
        using var cleanup = package;
        harness.Service.Install(package.Root, HardeningHarness.Now);
        store.Fault = StoreFault.SaveFails;
        harness.Trust.Accept(harness.Trust.Revoke(2, publishers: AcmeRevoked));

        var stillBroken = harness.Service.RetryPersistRevocations();
        Assert.False(stillBroken.Persisted);
        Assert.Equal(ComponentRefusalCodes.RevocationNotPersisted, stillBroken.Refusal!.Code);
        Assert.False(harness.Service.RevocationsPersisted);

        store.Fault = StoreFault.None;
        var saved = harness.Service.RetryPersistRevocations();

        Assert.True(saved.Persisted);
        Assert.Null(saved.Refusal);
        Assert.True(harness.Service.RevocationsPersisted);
        Assert.Null(harness.Service.RevocationPersistenceFailure);
        Assert.True(store.TryLoad(out var state, out _));
        var disabled = Assert.Single(state.Disabled);
        Assert.Equal(manifest.SignedBytesSha256(), disabled.Digest);

        // Retrying again does not duplicate the entry.
        Assert.True(harness.Service.RetryPersistRevocations().Persisted);
        store.TryLoad(out var again, out _);
        Assert.Single(again.Disabled);

        // The block is lifted: an unrelated, non-revoked component installs.
        var (other, _, _) = harness.Build(s => s.ComponentId = "acme.other");
        using var otherCleanup = other;
        Assert.NotEqual(
            ComponentRefusalCodes.RevocationNotPersisted,
            harness.Service.Install(other.Root, HardeningHarness.Now).Refusal?.Code);
    }

    [Fact]
    public void ARevocationThatIsPersistedLeavesInstallsOpenAndReportsNoFailure()
    {
        var store = new FaultyStateStore();
        using var harness = new HardeningHarness(store);
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        harness.Service.Install(package.Root, HardeningHarness.Now);

        harness.Trust.Accept(harness.Trust.Revoke(2, publishers: AcmeRevoked));

        var outcome = Assert.Single(harness.Service.RevocationOutcomes);
        Assert.Null(outcome.PersistenceFailure);
        Assert.True(harness.Service.RevocationsPersisted);
        Assert.Null(harness.Service.RevocationPersistenceFailure);
        Assert.True(store.TryLoad(out var state, out _));
        Assert.Single(state.Disabled);
    }

    [AvaloniaFact]
    public void RevokingAComponentThatRanSetsRestartRequiredAndRaisesThePromptHook()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        var loaded = harness.Service.Install(package.Root, HardeningHarness.Now).Component!;
        _ = loaded.Create(TestDescriptors.Context());
        var prompts = new List<RestartRequiredEventArgs>();
        harness.Service.RestartRequested += (_, e) => prompts.Add(e);

        harness.Trust.Accept(harness.Trust.Revoke(2, publishers: AcmeRevoked));

        Assert.True(harness.Service.RestartRequired);
        var outcome = Assert.Single(harness.Service.RevocationOutcomes);
        Assert.True(outcome.Unloaded);
        Assert.True(outcome.RestartRequired);
        var prompt = Assert.Single(prompts);
        Assert.Equal(ComponentRefusalCodes.RestartRequired, prompt.Code);
        Assert.Equal("acme.rating", prompt.ComponentId);
    }

    [Fact]
    public void ARevocationOfAComponentThatNeverRanDoesNotAskForARestart()
    {
        using var harness = new HardeningHarness();
        var (package, _, _) = harness.Build();
        using var cleanup = package;
        harness.Service.Install(package.Root, HardeningHarness.Now);
        var prompts = 0;
        harness.Service.RestartRequested += (_, _) => prompts++;

        harness.Trust.Accept(harness.Trust.Revoke(2, publishers: AcmeRevoked));

        Assert.False(harness.Service.RestartRequired);
        Assert.Equal(0, prompts);
    }
}

/// <summary>How a <see cref="FaultyStateStore"/> misbehaves.</summary>
public enum StoreFault
{
    /// <summary>It works.</summary>
    None,

    /// <summary>TrySave returns false.</summary>
    SaveFails,

    /// <summary>TryLoad returns false.</summary>
    LoadFails,

    /// <summary>TrySave throws.</summary>
    SaveThrows,

    /// <summary>TryLoad throws.</summary>
    LoadThrows,
}

/// <summary>An in-memory store that can be told to fail or throw, so the host's reaction can be tested.</summary>
internal sealed class FaultyStateStore : IComponentStateStore
{
    private readonly InMemoryComponentStateStore _inner = new();

    public StoreFault Fault { get; set; }

    public bool TryLoad(out ComponentInstallState state, out string? problem)
    {
        switch (Fault)
        {
            case StoreFault.LoadFails:
                state = new ComponentInstallState();
                problem = "the install state file could not be read.";
                return false;
            case StoreFault.LoadThrows:
                throw new InvalidOperationException("boom: C:\\secret\\path");
            default:
                return _inner.TryLoad(out state, out problem);
        }
    }

    public bool TrySave(ComponentInstallState state, out string? problem)
    {
        switch (Fault)
        {
            case StoreFault.SaveFails:
                problem = "the install state file could not be written.";
                return false;
            case StoreFault.SaveThrows:
                throw new InvalidOperationException("boom: C:\\secret\\path");
            default:
                return _inner.TrySave(state, out problem);
        }
    }
}
