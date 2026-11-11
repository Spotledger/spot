using System.Diagnostics;
using System.Globalization;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Component.Contract;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Install, upgrade and uninstall, and the teardown each one owes: instances disposed, the host's
/// callbacks gone, the catalogue entry cleared and the collectible context proved collected.
/// <para>
/// The unload test keeps a <see cref="WeakReference"/> to the context and nothing else, and it runs
/// the install and the uninstall inside a method that is never inlined: a local the JIT keeps alive
/// would keep a collectible context alive, and the test would then fail for a reason that has
/// nothing to do with the host.
/// </para>
/// </summary>
public sealed class LifecycleTests
{
    private static readonly Vectors VectorSet = Vectors.Load();

    /// <summary>What the log records for one load of the sample component.</summary>
    private static readonly string[] LoadedSample = { "acme.rating@1.0.0" };

    /// <summary>What the log records for one uninstall of the sample component.</summary>
    private static readonly string[] UnloadedSample = { "acme.rating@1.0.0" };

    [Fact]
    public void InstallRegistersTheComponentAndLogsIt()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        var result = trust.Service.Install(package.Root, TestHarness.Now);

        Assert.True(result.IsLoaded, result.Refusal?.ToString());
        Assert.Equal("acme.rating", result.Component!.ComponentId);
        Assert.Equal(1, trust.Service.Catalogue.Count);
        Assert.True(trust.Service.Catalogue.TryResolve("acme.rating", out var installed));
        Assert.Same(result.Component, installed);
        Assert.Equal(LoadedSample, trust.Log.LoadedIds);
        Assert.Equal("acme.rating@1.0.0", trust.Service.DescribeInstalled());
    }

    [Fact]
    public void InstallingTheSameComponentTwiceIsRefused()
    {
        using var trust = new TestHarness();
        using var first = new TempPackage();
        using var second = new TempPackage();
        PackageBuilder.Build(first.Root, PackageBuilder.SampleSpec(trust.Trust));
        PackageBuilder.Build(second.Root, PackageBuilder.SampleSpec(trust.Trust));

        Assert.True(trust.Service.Install(first.Root, TestHarness.Now).IsLoaded);
        var duplicate = trust.Service.Install(second.Root, TestHarness.Now);

        Assert.False(duplicate.IsLoaded);
        Assert.Equal("component.id.duplicate", duplicate.Refusal!.Code);
        Assert.Equal(1, trust.Service.Catalogue.Count);
    }

    [AvaloniaFact]
    public void UninstallDisposesTheInstanceAndStopsTheHostCallbacks()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));
        var loaded = trust.Service.Install(package.Root, TestHarness.Now).Component!;

        var edits = 0;
        var instance = loaded.Create(
            TestDescriptors.Context(),
            new ComponentCallbacks(ValueChanged: _ => edits++));
        var view = instance.View;
        Assert.Equal("AcmeRatingView", view.GetType().Name);

        // A user edit reaches the host's handler while the component is installed...
        PressRight(view);
        Assert.Equal(1, edits);

        var uninstall = trust.Service.Uninstall("acme.rating");

        Assert.True(uninstall.Removed);
        Assert.Equal(1, uninstall.DisposedInstances);
        Assert.Null(uninstall.Refusal);
        Assert.True(instance.IsDisposed);
        Assert.True(loaded.IsUnloaded);
        Assert.Equal(0, trust.Service.Catalogue.Count);
        Assert.Equal(UnloadedSample, trust.Log.UnloadedIds);
        Assert.False(trust.Service.Catalogue.TryResolve("acme.rating", out _));

        // ...and not after it: the instance unsubscribed the host's handlers before disposing.
        PressRight(view);
        Assert.Equal(1, edits);
    }

    [Fact]
    public void UninstallingSomethingThatIsNotInstalledIsRefused()
    {
        using var trust = new TestHarness();

        var uninstall = trust.Service.Uninstall("acme.rating");

        Assert.False(uninstall.Removed);
        Assert.Equal(0, uninstall.DisposedInstances);
        Assert.Equal(ComponentRefusalCodes.ComponentUnknown, uninstall.Refusal!.Code);
    }

    [Fact]
    public void TheContextIsCollectedAfterUninstall()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        // The teardown is measured in a child process: the xUnit v3 runner keeps the assemblies a test
        // loads, so a context a test unloads is never collected inside it. probes/UnloadProbe does the
        // same install, uninstall and weak-reference check in a plain process.
        var (exitCode, output) = RunProbe(
            UnloadProbePath(),
            package.Root,
            Path.Combine(AppContext.BaseDirectory, "Vectors", "component-signing-vectors.json"));

        Assert.True(exitCode == 0, "the unload probe exited " + exitCode + ": " + output);
        Assert.Contains("collected", output, StringComparison.Ordinal);
        Assert.Contains("0 component(s) left", output, StringComparison.Ordinal);
    }

    [Fact]
    public void AFailedUpgradeLeavesTheWorkingVersionInstalled()
    {
        using var trust = new TestHarness();
        using var working = new TempPackage();
        using var broken = new TempPackage();
        PackageBuilder.Build(working.Root, PackageBuilder.SampleSpec(trust.Trust));

        var spec = PackageBuilder.SampleSpec(trust.Trust);
        spec.SigningSeed = trust.Trust.OutsiderSeed;
        PackageBuilder.Build(broken.Root, spec);

        var installed = trust.Service.Install(working.Root, TestHarness.Now);
        Assert.True(installed.IsLoaded);

        var upgrade = trust.Service.Upgrade(broken.Root, TestHarness.Now);

        Assert.False(upgrade.IsLoaded);
        Assert.Equal(ComponentRefusalCodes.SignatureInvalid, upgrade.Refusal!.Code);
        Assert.Equal(1, trust.Service.Catalogue.Count);
        Assert.True(trust.Service.Catalogue.TryResolve("acme.rating", out var still));
        Assert.Same(installed.Component, still);
        Assert.False(installed.Component!.IsUnloaded);
        Assert.Empty(trust.Log.UnloadedIds);
    }

    [Fact]
    public void AnUpgradeReplacesTheInstalledVersionAndUnloadsTheOldContext()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        var first = trust.Service.Install(package.Root, TestHarness.Now);
        Assert.True(first.IsLoaded);

        // A second Install of the same id is the duplicate case; Upgrade is the path that replaces it.
        var upgrade = trust.Service.Upgrade(package.Root, TestHarness.Now);

        Assert.True(upgrade.IsLoaded, upgrade.Refusal?.ToString());
        Assert.Equal(1, trust.Service.Catalogue.Count);
        Assert.True(first.Component!.IsUnloaded);
        Assert.NotSame(first.Component, upgrade.Component);
        Assert.Equal(UnloadedSample, trust.Log.UnloadedIds);
    }

    [Fact]
    public void UninstallAllClearsEveryComponent()
    {
        using var trust = new TestHarness();
        using var package = new TempPackage();
        PackageBuilder.Build(package.Root, PackageBuilder.SampleSpec(trust.Trust));

        Assert.True(trust.Service.Install(package.Root, TestHarness.Now).IsLoaded);
        trust.Service.UninstallAll();

        Assert.Equal(0, trust.Service.Catalogue.Count);
        Assert.Equal("no components installed", trust.Service.DescribeInstalled());
        Assert.Single(trust.Log.UnloadedIds);
    }

    /// <summary>Sends an arrow key to the rating view, which is how a user edits it.</summary>
    /// <param name="view">The view.</param>
    private static void PressRight(Control view) =>
        view.RaiseEvent(new KeyEventArgs { RoutedEvent = InputElement.KeyDownEvent, Key = Key.Right });

    /// <summary>The compiled unload probe, in the configuration this test was built in.</summary>
    private static string UnloadProbePath()
    {
        // bin/<configuration>/net10.0 -> the test project's directory.
        var projectDirectory = Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "..", ".."));
        var configuration = new DirectoryInfo(AppContext.BaseDirectory).Parent?.Name
            ?? throw new InvalidOperationException("The test output directory has a configuration name.");
        var binaries = Path.Combine(projectDirectory, "probes", "UnloadProbe", "bin");
        var candidates = Directory.Exists(binaries)
            ? Directory.GetFiles(binaries, "UnloadProbe.dll", SearchOption.AllDirectories)
                .Where(path => path.Contains(
                    Path.DirectorySeparatorChar + configuration + Path.DirectorySeparatorChar,
                    StringComparison.Ordinal))
                .OrderBy(path => path, StringComparer.Ordinal)
                .ToArray()
            : Array.Empty<string>();
        return candidates.Length > 0
            ? candidates[0]
            : throw new InvalidOperationException(
                "The unload probe was not built (" + configuration + "): " + binaries + " has no UnloadProbe.dll.");
    }

    /// <summary>Runs the probe and returns its exit code and its combined output.</summary>
    /// <param name="probe">The probe's assembly.</param>
    /// <param name="packageRoot">The package to install and uninstall.</param>
    /// <param name="vectors">The vector file the probe takes its trust material from.</param>
    private static (int ExitCode, string Output) RunProbe(string probe, string packageRoot, string vectors)
    {
        var start = new ProcessStartInfo(Environment.GetEnvironmentVariable("DOTNET_HOST_PATH") ?? "dotnet")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
        };
        start.ArgumentList.Add("exec");
        start.ArgumentList.Add(probe);
        start.ArgumentList.Add(packageRoot);
        start.ArgumentList.Add(vectors);
        start.ArgumentList.Add(TestHarness.Now.ToString(CultureInfo.InvariantCulture));

        using var process = Process.Start(start) ?? throw new InvalidOperationException("The unload probe did not start.");
        var output = process.StandardOutput.ReadToEnd() + process.StandardError.ReadToEnd();
        Assert.True(process.WaitForExit(120_000), "the unload probe did not finish");
        return (process.ExitCode, output);
    }

    /// <summary>The trust material and the install service one lifecycle test needs.</summary>
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
