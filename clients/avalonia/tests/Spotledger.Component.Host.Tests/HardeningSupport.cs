using Spotledger.Component.Host;

namespace Spotledger.Component.Host.Tests;

/// <summary>The trust, log, state store and service every hardening test starts from.</summary>
internal sealed class HardeningHarness : IDisposable
{
    /// <summary>The instant every test verifies at: inside the certificate's window.</summary>
    public const long Now = 1_760_000_000;

    private static readonly Vectors VectorSet = Vectors.Load();

    public HardeningHarness(IComponentStateStore? state = null)
    {
        Trust = new TestTrust(VectorSet);
        Trust.TrustPublisher();
        Log = new RecordingLog();
        State = state ?? new InMemoryComponentStateStore();
        Service = new ComponentInstallationService(Trust.Store, Log, stateStore: State);
    }

    public TestTrust Trust { get; }

    public RecordingLog Log { get; }

    public IComponentStateStore State { get; }

    public ComponentInstallationService Service { get; }

    public static Vectors Vectors => VectorSet;

    /// <summary>Builds the sound sample package into a fresh temporary directory.</summary>
    public (TempPackage Package, ComponentManifest Manifest, PackageSpec Spec) Build(Action<PackageSpec>? tune = null)
    {
        var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(Trust);
        tune?.Invoke(spec);
        var manifest = PackageBuilder.Build(package.Root, spec);
        return (package, manifest, spec);
    }

    /// <summary>Records that a higher version (or generation) was installed before, as a previous run would have.</summary>
    public void Seed(string componentId, string? highestVersion = null, ulong? highestGeneration = null)
    {
        State.TryLoad(out var state, out _);
        if (highestVersion is not null)
        {
            state.HighestVersion[componentId] = highestVersion;
        }

        if (highestGeneration is { } generation)
        {
            state.HighestGeneration[componentId] = generation;
        }

        State.TrySave(state, out _);
    }

    public void Dispose() => Service.UninstallAll();
}

/// <summary>A state store whose writes fail: the host must then refuse the install.</summary>
internal sealed class UnwritableStateStore : IComponentStateStore
{
    public bool TryLoad(out ComponentInstallState state, out string? problem)
    {
        state = new ComponentInstallState();
        problem = null;
        return true;
    }

    public bool TrySave(ComponentInstallState state, out string? problem)
    {
        problem = "the install state file could not be written.";
        return false;
    }
}
