using System.Globalization;
using System.Runtime.CompilerServices;
using System.Text.Json;
using Spotledger.Component.Host;

namespace UnloadProbe;

/// <summary>
/// Installs a signed component package, uninstalls it, and reports whether the component's collectible
/// load context was really collected.
/// <para>
/// This runs as its own process on purpose. The collectible-context teardown is the one property a
/// test host cannot show: the xUnit v3 runner keeps the assemblies a test loads, so a context a test
/// unloads is never collected inside it — not because the host under test leaks, but because the
/// runner holds the assembly. In a plain process the same install and uninstall collects, which is
/// what this program measures.
/// </para>
/// <para>
/// Usage: <c>UnloadProbe &lt;package-directory&gt; &lt;vectors.json&gt; [&lt;utc-seconds&gt;]</c>. It
/// prints one line and exits 0 when the context was collected and the catalogue is empty, 1 when the
/// context outlived the uninstall, 2 on bad arguments or a missing vector file, 3 when the install or
/// the uninstall was refused.
/// </para>
/// </summary>
public static class Program
{
    /// <summary>Runs the probe.</summary>
    /// <param name="args">The package directory, the vector file, and optionally the instant.</param>
    /// <returns>0 when the context was collected.</returns>
    public static int Main(string[] args)
    {
        ArgumentNullException.ThrowIfNull(args);
        if (args.Length < 2)
        {
            Console.Out.WriteLine("usage: UnloadProbe <package-directory> <vectors.json> [<utc-seconds>]");
            return 2;
        }

        var now = args.Length > 2
            && long.TryParse(args[2], NumberStyles.Integer, CultureInfo.InvariantCulture, out var parsed)
                ? parsed
                : 1_760_000_000;

        var trust = TrustFromVectors(args[1], now);
        if (trust is null)
        {
            Console.Out.WriteLine("the vector file could not be read: " + args[1]);
            return 2;
        }

        var service = new ComponentInstallationService(trust);
        var weak = InstallAndUninstall(service, args[0], now);
        Sweep();

        if (weak.IsAlive)
        {
            Console.Out.WriteLine("alive: the context still holds the component's code");
            return 1;
        }

        var left = service.Catalogue.Count;
        Console.Out.WriteLine(
            "collected: the context was unloaded and collected, "
            + left.ToString(CultureInfo.InvariantCulture) + " component(s) left");
        return left == 0 ? 0 : 1;
    }

    /// <summary>
    /// The install and the uninstall, in a method the JIT does not inline: a local of the caller would
    /// keep the context alive and the measurement would be about this program, not about the host.
    /// </summary>
    /// <param name="service">The install service.</param>
    /// <param name="directory">The package directory.</param>
    /// <param name="now">The instant to verify at.</param>
    [MethodImpl(MethodImplOptions.NoInlining)]
    private static WeakReference InstallAndUninstall(ComponentInstallationService service, string directory, long now)
    {
        var installed = service.Install(directory, now);
        if (!installed.IsLoaded)
        {
            Console.Out.WriteLine("install refused: " + installed.Refusal);
            Environment.Exit(3);
        }

        var weak = service.UninstallAndTrack(installed.Component!.ComponentId, out var result);
        if (!result.Removed)
        {
            Console.Out.WriteLine("uninstall refused: " + result.Refusal);
            Environment.Exit(3);
        }

        return weak;
    }

    /// <summary>Collects until the runtime has had several chances to finish the unload.</summary>
    private static void Sweep()
    {
        for (var attempt = 0; attempt < 10; attempt++)
        {
            GC.Collect();
            GC.WaitForPendingFinalizers();
            GC.Collect();
        }
    }

    /// <summary>The trust material the vectors describe: one store root, one publisher certificate.</summary>
    /// <param name="path">The vector file.</param>
    /// <param name="now">The instant to accept the certificate at.</param>
    private static ComponentTrustStore? TrustFromVectors(string path, long now)
    {
        if (!File.Exists(path))
        {
            return null;
        }

        using var document = JsonDocument.Parse(File.ReadAllText(path));
        var keys = document.RootElement.GetProperty("keys");
        var rootKey = Convert.FromBase64String(keys.GetProperty("root").GetProperty("public_key_b64").GetString()!);
        var rootSeed = Convert.FromHexString(keys.GetProperty("root").GetProperty("seed_hex").GetString()!);
        var publisherKey = Convert.FromBase64String(keys.GetProperty("publisher").GetProperty("public_key_b64").GetString()!);
        var rootId = Ed25519.KeyIdOf(rootKey);

        var store = new ComponentTrustStore();
        store.AddRoot(rootKey, 1_600_000_000, 2_000_000_000);

        var body = new PublisherCertificateBody(
            "acme", "Probe publisher", publisherKey, 1_700_000_000, 1_800_000_000, rootId, 1);
        var certificate = new SignedPublisherCertificate(body, Ed25519.Sign(rootSeed, body.SignedBytes()));
        var refusal = store.Trust(certificate, now);
        if (refusal is not null)
        {
            Console.Out.WriteLine("the publisher certificate was refused: " + refusal);
            return null;
        }

        return store;
    }
}
