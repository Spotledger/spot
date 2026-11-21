using System.Reflection;
using System.Runtime.Loader;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Dependency confinement: what a package may resolve, and what it may not.
/// <para>
/// The rule is a dependency rule and not a sandbox — an accepted component runs in this process — but
/// a package that reaches for the application's own assemblies is refused before a load context
/// exists, and the context refuses it again if resolution is attempted later.
/// </para>
/// </summary>
public sealed class ConfinementTests
{
    private static readonly Vectors VectorSet = Vectors.Load();

    [Fact]
    public void APackageThatReferencesTheApplicationsOwnCodeIsRefusedBeforeAnyLoad()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.Assembly = HostileProbeBytes();
        spec.AssemblyPath = "assemblies/HostileProbe.dll";
        spec.EntryAssembly = "assemblies/HostileProbe.dll";
        PackageBuilder.Build(package.Root, spec);

        var before = ContextsHolding("HostileProbe");
        var result = trust.Service.Install(package.Root, TestHarness.Now);

        Assert.False(result.IsLoaded, result.Refusal?.ToString() ?? "the hostile package loaded");
        Assert.Equal(ComponentRefusalCodes.HostPrivateDependency, result.Refusal!.Code);
        Assert.Contains("Spotledger.Client.Core", result.Refusal.Message, StringComparison.Ordinal);
        Assert.Equal(0, before);
        Assert.Equal(before, ContextsHolding("HostileProbe"));
        Assert.DoesNotContain(
            AppDomain.CurrentDomain.GetAssemblies(),
            static assembly => string.Equals(assembly.GetName().Name, "HostileProbe", StringComparison.Ordinal));
        Assert.Empty(trust.Log.LoadedIds);
    }

    [Fact]
    public void TheDependencyRuleNamesWhatIsPrivateAndWhatIsShared()
    {
        Assert.True(ComponentDependencies.IsHostPrivate("Spotledger.Client.Core"));
        Assert.True(ComponentDependencies.IsHostPrivate("Spotledger.Client.Api"));

        // The runtime binds simple names ignoring case, so a re-cased reference is the same assembly.
        Assert.True(ComponentDependencies.IsHostPrivate("SPOTLEDGER.Client.Core"));
        Assert.True(ComponentDependencies.IsHostPrivate("spotledger.client.api"));
        Assert.True(ComponentDependencies.IsHostPrivate("spotledger.component.contract"));
        Assert.False(ComponentDependencies.IsHostPrivate("Spotledger.Component.Contract"));
        Assert.False(ComponentDependencies.IsHostPrivate("System.Runtime"));
        Assert.False(ComponentDependencies.IsHostPrivate("Avalonia.Controls"));
        Assert.False(ComponentDependencies.IsHostPrivate("Acme.RatingComponent"));
        Assert.False(ComponentDependencies.IsHostPrivate(string.Empty));
        Assert.Contains(
            "Spotledger.Component.Contract",
            ComponentDependencies.SharedAssemblyNames,
            StringComparer.Ordinal);
    }

    [Fact]
    public void TheReferenceTableIsReadFromMetadataNotByRunningTheCode()
    {
        Assert.True(ComponentDependencies.TryReadReferences(PackageBuilder.SampleAssembly(), out var references));
        Assert.Contains(
            "Spotledger.Component.Contract",
            references,
            StringComparer.Ordinal);
        Assert.Contains(
            references,
            name => name.StartsWith("Avalonia", StringComparison.Ordinal));
        Assert.Null(ComponentDependencies.Check(PackageBuilder.SampleAssembly()));
    }

    [Fact]
    public void TheHostileProbeIsRefusedWithTheNameOfTheAssemblyItWanted()
    {
        var refusal = ComponentDependencies.Check(HostileProbeBytes());

        Assert.NotNull(refusal);
        Assert.Equal(ComponentRefusalCodes.HostPrivateDependency, refusal.Code);
        Assert.Contains("Spotledger.Client.Core", refusal.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void BytesThatAreNotAnAssemblyAreRefusedAsSuch()
    {
        var refusal = ComponentDependencies.Check("not an assembly"u8.ToArray());

        Assert.NotNull(refusal);
        Assert.Equal(ComponentRefusalCodes.AssemblyInvalid, refusal.Code);
    }

    [Fact]
    public void AContextRefusesAHostPrivateAssemblyAndRecordsIt()
    {
        var context = new ComponentLoadContext(
            "confinement",
            new Dictionary<string, Assembly>(StringComparer.Ordinal),
            new Dictionary<string, byte[]>(StringComparer.Ordinal));
        try
        {
            Assert.Throws<FileNotFoundException>(
                () => context.LoadFromAssemblyName(new AssemblyName("Spotledger.Client.Core")));
            Assert.Contains("Spotledger.Client.Core", context.RefusedAssemblies, StringComparer.Ordinal);
        }
        finally
        {
            context.Unload();
        }
    }

    [Fact]
    public void AContextAnswersTheContractFromTheHostsOwnCopy()
    {
        var context = new ComponentLoadContext(
            "confinement",
            new Dictionary<string, Assembly>(StringComparer.Ordinal)
            {
                [typeof(Spotledger.Component.Contract.IComponent).Assembly.GetName().Name!] =
                    typeof(Spotledger.Component.Contract.IComponent).Assembly,
            },
            new Dictionary<string, byte[]>(StringComparer.Ordinal)
            {
                ["Acme.RatingComponent"] = PackageBuilder.SampleAssembly(),
            });
        try
        {
            var assembly = context.LoadPackageAssembly("Acme.RatingComponent");
            var componentType = assembly.GetTypes().Single(t => t.Name == "AcmeRatingComponent");

            // Type identity, not a name match: the interface the package implements is the host's own
            // copy of the contract, which is what "shared" means.
            Assert.Contains(typeof(Spotledger.Component.Contract.IComponent), componentType.GetInterfaces());
            Assert.Same(
                typeof(Spotledger.Component.Contract.IComponent).Assembly,
                componentType.GetInterfaces().First(i => i.Name == "IComponent").Assembly);
            Assert.Empty(context.RefusedAssemblies);
        }
        finally
        {
            context.Unload();
        }
    }

    [Fact]
    public void LoadingAnAssemblyThePackageDoesNotShipIsARefusal()
    {
        var context = new ComponentLoadContext(
            "confinement",
            new Dictionary<string, Assembly>(StringComparer.Ordinal),
            new Dictionary<string, byte[]>(StringComparer.Ordinal));
        try
        {
            Assert.Throws<FileNotFoundException>(() => context.LoadPackageAssembly("Acme.RatingComponent"));
        }
        finally
        {
            context.Unload();
        }
    }

    private static byte[] HostileProbeBytes()
    {
        var path = Path.Combine(AppContext.BaseDirectory, "probes", "HostileProbe.dll");
        Assert.True(File.Exists(path), "the probe was not built next to the tests: " + path);
        return File.ReadAllBytes(path);
    }

    /// <summary>
    /// How many collectible load contexts hold an assembly named <paramref name="assemblyName"/>.
    /// <para>
    /// Counting <see cref="AppDomain.GetAssemblies"/> instead would be false evidence: reading a
    /// package's metadata lazily loads the runtime's own metadata assemblies, so the count moves for
    /// reasons that have nothing to do with the package. This counts only the loader's contexts, and
    /// other tests' contexts are excluded by name.
    /// </para>
    /// </summary>
    /// <param name="assemblyName">The simple assembly name to look for.</param>
    private static int ContextsHolding(string assemblyName) =>
        AppDomain.CurrentDomain.GetAssemblies()
            .Where(a => string.Equals(a.GetName().Name, assemblyName, StringComparison.Ordinal))
            .Select(AssemblyLoadContext.GetLoadContext)
            .Count(static context => context is { IsCollectible: true });

    /// <summary>The trust material and the install service the confinement tests need.</summary>
    private sealed class TestHarness : IDisposable
    {
        public TestHarness()
        {
            Trust = new TestTrust(VectorSet);
            Trust.TrustPublisher();
            Log = new RecordingLog();
            Service = new ComponentInstallationService(Trust.Store, Log);
        }

        /// <summary>The instant every test verifies at: inside the certificate's window.</summary>
        public static long Now => 1_760_000_000;

        public TestTrust Trust { get; }

        public RecordingLog Log { get; }

        public ComponentInstallationService Service { get; }

        public void Dispose() => Service.UninstallAll();
    }
}
