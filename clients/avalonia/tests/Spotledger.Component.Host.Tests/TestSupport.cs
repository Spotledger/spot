using System.Globalization;
using System.Text;
using System.Text.Json;
using Spotledger.Component.Host;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// The Rust-generated vectors: the keys, the certificate, the revocation list and the four
/// manifests a program linking <c>spot-bundle-signing</c> produced. Every test that signs or
/// verifies reads its material from here, so the C# side is never checked against itself.
/// </summary>
public sealed class Vectors
{
    private readonly JsonDocument _document;

    private Vectors(JsonDocument document)
    {
        _document = document;
    }

    /// <summary>Loads the checked-in vector file.</summary>
    public static Vectors Load()
    {
        var path = Path.Combine(AppContext.BaseDirectory, "Vectors", "component-signing-vectors.json");
        return new Vectors(JsonDocument.Parse(File.ReadAllText(path)));
    }

    /// <summary>The seed of a key the vectors name.</summary>
    /// <param name="name">root, publisher, revoked or outsider.</param>
    public byte[] Seed(string name) => Convert.FromHexString(Key(name).GetProperty("seed_hex").GetString()!);

    /// <summary>The public key of a key the vectors name.</summary>
    /// <param name="name">root, publisher, revoked or outsider.</param>
    public byte[] PublicKey(string name) => Convert.FromBase64String(Key(name).GetProperty("public_key_b64").GetString()!);

    /// <summary>The key id of a key the vectors name.</summary>
    /// <param name="name">root, publisher, revoked or outsider.</param>
    public string KeyId(string name) => Key(name).GetProperty("key_id").GetString()!;

    /// <summary>The Rust-signed publisher certificate.</summary>
    public JsonElement Certificate => _document.RootElement.GetProperty("certificate");

    /// <summary>The Rust-signed revocation list.</summary>
    public JsonElement Revocation => _document.RootElement.GetProperty("revocation");

    /// <summary>One of the four manifests: plain, tricky_strings, compromised_signer or wrong_key.</summary>
    /// <param name="name">The manifest's name in the vector file.</param>
    public JsonElement Manifest(string name) => _document.RootElement.GetProperty("manifests").GetProperty(name);

    /// <summary>The optimum solver is not needed; this is the raw JSON text of a vector subtree.</summary>
    /// <param name="element">The element.</param>
    public static string Text(JsonElement element) => element.GetRawText();

    private JsonElement Key(string name) => _document.RootElement.GetProperty("keys").GetProperty(name);
}

/// <summary>
/// The test keys, certificates and revocation lists built from the vectors' seeds. The seeds are
/// public test material: nothing here is a production key, and nothing is logged.
/// </summary>
public sealed class TestTrust
{
    private readonly Vectors _vectors;

    /// <summary>Builds the trust material from the vector seeds.</summary>
    /// <param name="vectors">The vectors.</param>
    public TestTrust(Vectors vectors)
    {
        ArgumentNullException.ThrowIfNull(vectors);
        _vectors = vectors;
        Store = new ComponentTrustStore();
        Root = Store.AddRoot(vectors.PublicKey("root"), 1_600_000_000, 2_000_000_000);
    }

    /// <summary>The store under test.</summary>
    public ComponentTrustStore Store { get; }

    /// <summary>The store root.</summary>
    public TrustRoot Root { get; }

    /// <summary>The publisher's signing seed.</summary>
    public byte[] PublisherSeed => _vectors.Seed("publisher");

    /// <summary>The publisher's key id.</summary>
    public string PublisherKeyId => _vectors.KeyId("publisher");

    /// <summary>The compromised key's seed.</summary>
    public byte[] RevokedSeed => _vectors.Seed("revoked");

    /// <summary>The compromised key's id.</summary>
    public string RevokedKeyId => _vectors.KeyId("revoked");

    /// <summary>A key no certificate names.</summary>
    public byte[] OutsiderSeed => _vectors.Seed("outsider");

    /// <summary>A key no certificate names.</summary>
    public string OutsiderKeyId => _vectors.KeyId("outsider");

    /// <summary>Issues a publisher certificate signed by the store root.</summary>
    /// <param name="publisherId">The publisher the certificate names.</param>
    /// <param name="seed">The publisher's signing seed.</param>
    /// <param name="validFrom">First valid instant (UTC seconds).</param>
    /// <param name="validTo">First invalid instant (UTC seconds).</param>
    /// <param name="serial">The issuer's serial.</param>
    public SignedPublisherCertificate Issue(
        string publisherId,
        byte[] seed,
        long validFrom = 1_700_000_000,
        long validTo = 1_800_000_000,
        long serial = 1)
    {
        var body = new PublisherCertificateBody(
            publisherId,
            "Test publisher",
            Ed25519.PublicKeyOf(seed),
            validFrom,
            validTo,
            Root.KeyId,
            serial);
        return new SignedPublisherCertificate(body, Ed25519.Sign(_vectors.Seed("root"), body.SignedBytes()));
    }

    /// <summary>Trusts the standard certificate for the publisher key.</summary>
    /// <param name="now">The instant to accept it at.</param>
    public SignedPublisherCertificate TrustPublisher(long now = 1_760_000_000)
    {
        var certificate = Issue("acme", PublisherSeed);
        var refusal = Store.Trust(certificate, now);
        if (refusal is not null)
        {
            throw new InvalidOperationException(refusal.ToString());
        }

        return certificate;
    }

    /// <summary>A revocation list signed by the store root.</summary>
    /// <param name="generation">The list's generation.</param>
    /// <param name="keys">Revoked keys.</param>
    /// <param name="publishers">Revoked publishers.</param>
    /// <param name="packages">Revoked package digests.</param>
    public RevocationList Revoke(
        ulong generation = 1,
        IEnumerable<RevokedKeyEntry>? keys = null,
        IEnumerable<RevokedPublisherEntry>? publishers = null,
        IEnumerable<RevokedPackageEntry>? packages = null)
    {
        var body = new RevocationBody(
            generation,
            1_760_000_000,
            Root.KeyId,
            keys ?? Array.Empty<RevokedKeyEntry>(),
            publishers ?? Array.Empty<RevokedPublisherEntry>(),
            packages ?? Array.Empty<RevokedPackageEntry>());
        return new RevocationList(body, Ed25519.Sign(_vectors.Seed("root"), body.SignedBytes()));
    }

    /// <summary>Accepts a revocation list, failing the test when the store refuses it.</summary>
    /// <param name="list">The list.</param>
    /// <param name="now">The instant to check the signing root at.</param>
    public void Accept(RevocationList list, long now = 1_760_000_000)
    {
        var refusal = Store.AcceptRevocationList(list, now);
        if (refusal is not null)
        {
            throw new InvalidOperationException(refusal.ToString());
        }
    }
}

/// <summary>What a built test package carries; every field has the value a sound package needs.</summary>
public sealed class PackageSpec
{
    /// <summary>The manifest's component id.</summary>
    public string ComponentId { get; set; } = "acme.rating";

    /// <summary>The manifest's display name.</summary>
    public string DisplayName { get; set; } = "Acme star rating";

    /// <summary>The manifest's component version.</summary>
    public string ComponentVersion { get; set; } = "1.0.0";

    /// <summary>The manifest's publisher.</summary>
    public string PublisherId { get; set; } = "acme";

    /// <summary>The contract range's minimum.</summary>
    public string Minimum { get; set; } = "1.0";

    /// <summary>The contract range's maximum, exclusive.</summary>
    public string MaximumExclusive { get; set; } = "2.0";

    /// <summary>When the publisher signed (UTC seconds).</summary>
    public long SignedAt { get; set; } = 1_760_000_000;

    /// <summary>The key id the manifest names.</summary>
    public string KeyId { get; set; } = string.Empty;

    /// <summary>The seed the package is signed with.</summary>
    public byte[] SigningSeed { get; set; } = Array.Empty<byte>();

    /// <summary>The assembly bytes the package ships.</summary>
    public byte[] Assembly { get; set; } = Array.Empty<byte>();

    /// <summary>The package-relative path of the assembly.</summary>
    public string AssemblyPath { get; set; } = "assemblies/Acme.RatingComponent.dll";

    /// <summary>The assembly the host loads.</summary>
    public string EntryAssembly { get; set; } = "assemblies/Acme.RatingComponent.dll";

    /// <summary>Whether the signature file is written at all.</summary>
    public bool WriteSignature { get; set; } = true;

    /// <summary>Replaces the signature's bytes, when a test needs a bad one.</summary>
    public byte[]? SignatureOverride { get; set; }

    /// <summary>Further assemblies the manifest lists next to the entry assembly (package path to bytes).</summary>
    public Dictionary<string, byte[]> ExtraAssemblies { get; } = new(StringComparer.Ordinal);

    /// <summary>Adds an asset to the manifest.</summary>
    public Dictionary<string, byte[]> Assets { get; } = new(StringComparer.Ordinal);

    /// <summary>Extra members merged into the manifest object, for refusal tests.</summary>
    public Dictionary<string, string> ExtraManifestMembers { get; } = new(StringComparer.Ordinal);
}

/// <summary>Builds signed packages on disk for the tests, and the sample's own bytes.</summary>
public static class PackageBuilder
{
    /// <summary>The compiled sample component, the payload every sound package ships.</summary>
    public static byte[] SampleAssembly() =>
        File.ReadAllBytes(typeof(Acme.RatingComponent.AcmeRatingComponent).Assembly.Location);

    /// <summary>A sound spec for the sample component, signed by the publisher key.</summary>
    /// <param name="trust">The trust material whose publisher key signs.</param>
    public static PackageSpec SampleSpec(TestTrust trust)
    {
        ArgumentNullException.ThrowIfNull(trust);
        return new PackageSpec
        {
            KeyId = trust.PublisherKeyId,
            SigningSeed = trust.PublisherSeed,
            Assembly = SampleAssembly(),
        };
    }

    /// <summary>Writes a package into <paramref name="root"/> and returns its manifest.</summary>
    /// <param name="root">The package directory (created).</param>
    /// <param name="spec">What the package carries.</param>
    public static ComponentManifest Build(string root, PackageSpec spec)
    {
        ArgumentNullException.ThrowIfNull(spec);
        Directory.CreateDirectory(root);
        var assemblyPath = Path.Combine(root, spec.AssemblyPath.Replace('/', Path.DirectorySeparatorChar));
        Directory.CreateDirectory(Path.GetDirectoryName(assemblyPath)!);
        File.WriteAllBytes(assemblyPath, spec.Assembly);

        var assemblies = new List<(string Path, byte[] Bytes)> { (spec.AssemblyPath, spec.Assembly) };
        foreach (var (extraPath, extraBytes) in spec.ExtraAssemblies)
        {
            var extra = Path.Combine(root, extraPath.Replace('/', Path.DirectorySeparatorChar));
            Directory.CreateDirectory(Path.GetDirectoryName(extra)!);
            File.WriteAllBytes(extra, extraBytes);
            assemblies.Add((extraPath, extraBytes));
        }

        foreach (var (path, bytes) in spec.Assets)
        {
            var absolute = Path.Combine(root, path.Replace('/', Path.DirectorySeparatorChar));
            Directory.CreateDirectory(Path.GetDirectoryName(absolute)!);
            File.WriteAllBytes(absolute, bytes);
        }

        var json = new StringBuilder();
        json.Append('{');
        json.Append("\"component_id\":").Append(Quote(spec.ComponentId)).Append(',');
        json.Append("\"component_version\":").Append(Quote(spec.ComponentVersion)).Append(',');
        json.Append("\"display_name\":").Append(Quote(spec.DisplayName)).Append(',');
        json.Append("\"publisher_id\":").Append(Quote(spec.PublisherId)).Append(',');
        json.Append("\"entry_assembly\":").Append(Quote(spec.EntryAssembly)).Append(',');
        json.Append("\"key_id\":").Append(Quote(spec.KeyId)).Append(',');
        json.Append("\"signed_at\":").Append(spec.SignedAt.ToString(CultureInfo.InvariantCulture)).Append(',');
        json.Append("\"contract_range\":{\"minimum\":").Append(Quote(spec.Minimum))
            .Append(",\"maximum_exclusive\":").Append(Quote(spec.MaximumExclusive)).Append("},");
        json.Append("\"assemblies\":[");
        json.Append(string.Join(",", assemblies.Select(a =>
            "{\"path\":" + Quote(a.Path) + ",\"sha256\":" + Quote(ComponentPackage.HashFile(a.Bytes)) + "}")));
        json.Append("],\"assets\":[");
        json.Append(string.Join(",", spec.Assets.Select(a =>
            "{\"path\":" + Quote(a.Key) + ",\"sha256\":" + Quote(ComponentPackage.HashFile(a.Value)) + "}")));
        json.Append(']');
        foreach (var (name, value) in spec.ExtraManifestMembers)
        {
            json.Append(',').Append(Quote(name)).Append(':').Append(value);
        }

        json.Append('}');
        File.WriteAllText(Path.Combine(root, ComponentPackagePaths.ManifestFileName), json.ToString());

        if (!ComponentManifest.TryParse(json.ToString(), out var manifest, out var problems) || manifest is null)
        {
            throw new InvalidOperationException("The test manifest is not sound: " + string.Join(" ", problems));
        }

        if (spec.WriteSignature)
        {
            var signature = spec.SignatureOverride ?? Ed25519.Sign(spec.SigningSeed, manifest.SignedBytes());
            File.WriteAllText(
                Path.Combine(root, ComponentPackagePaths.SignatureFileName),
                Convert.ToBase64String(signature));
        }

        return manifest;
    }

    private static string Quote(string value) => JsonSerializer.Serialize(value);
}

/// <summary>A temporary package directory, removed when the test ends.</summary>
public sealed class TempPackage : IDisposable
{
    /// <summary>Creates the directory under the system's temporary path.</summary>
    public TempPackage()
    {
        Root = Path.Combine(Path.GetTempPath(), "spot-component-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(Root);
    }

    /// <summary>The directory.</summary>
    public string Root { get; }

    /// <inheritdoc />
    public void Dispose()
    {
        try
        {
            Directory.Delete(Root, recursive: true);
        }
        catch (IOException)
        {
            // A locked file on a slow machine: the temporary directory is the platform's to reap.
        }
    }
}
