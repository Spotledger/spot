using System.Globalization;
using System.Text.Json;
using System.Text.RegularExpressions;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Host;

/// <summary>One file a manifest lists: its package-relative path and the SHA-256 of its bytes.</summary>
/// <param name="Path">The path, relative to the package root, with forward slashes.</param>
/// <param name="Sha256">The SHA-256 of the file's bytes, 64 lower-case hex characters.</param>
public sealed record ManifestFile(string Path, string Sha256);

/// <summary>
/// A package's <c>component.json</c>: the identity of the component, the contract range it targets,
/// the assembly the host loads, every file it ships with a hash, and the signing facts. The signed
/// bytes are <see cref="CanonicalJson.ComponentManifestDomain"/> followed by the canonical JSON of
/// <see cref="ToCanonical"/> — never the file's own bytes, so whitespace and key order on disk do
/// not matter and the Rust side can reproduce the same bytes from the same fields.
/// </summary>
public sealed partial class ComponentManifest
{
    private ComponentManifest(
        string componentId,
        string displayName,
        ComponentVersion version,
        string publisherId,
        ContractVersionRange contractRange,
        string entryAssembly,
        IReadOnlyList<ManifestFile> assemblies,
        IReadOnlyList<ManifestFile> assets,
        string keyId,
        long signedAt)
    {
        ComponentId = componentId;
        DisplayName = displayName;
        Version = version;
        PublisherId = publisherId;
        ContractRange = contractRange;
        EntryAssembly = entryAssembly;
        Assemblies = assemblies;
        Assets = assets;
        KeyId = keyId;
        SignedAt = signedAt;
    }

    /// <summary>The component's id, for example <c>acme.rating</c>.</summary>
    public string ComponentId { get; }

    /// <summary>The name a picker shows.</summary>
    public string DisplayName { get; }

    /// <summary>The component's own version.</summary>
    public ComponentVersion Version { get; }

    /// <summary>The publisher that signed the package.</summary>
    public string PublisherId { get; }

    /// <summary>The contract range the component targets.</summary>
    public ContractVersionRange ContractRange { get; }

    /// <summary>The package-relative path of the assembly that holds the component.</summary>
    public string EntryAssembly { get; }

    /// <summary>Every assembly in the package, sorted by path.</summary>
    public IReadOnlyList<ManifestFile> Assemblies { get; }

    /// <summary>Every asset in the package, sorted by path.</summary>
    public IReadOnlyList<ManifestFile> Assets { get; }

    /// <summary>The signing key's id.</summary>
    public string KeyId { get; }

    /// <summary>When the publisher signed (UTC seconds).</summary>
    public long SignedAt { get; }

    /// <summary>Every listed file, assemblies then assets, sorted by path within each list.</summary>
    public IEnumerable<ManifestFile> Files => Assemblies.Concat(Assets);

    /// <summary>The canonical JSON the signature covers, without the domain prefix.</summary>
    public CanonicalValue ToCanonical()
    {
        var range = CanonicalObject.Of(
            ("maximum_exclusive", CanonicalJson.Text(ContractRange.MaximumExclusive.ToString())),
            ("minimum", CanonicalJson.Text(ContractRange.Minimum.ToString())));

        return CanonicalObject.Of(
            ("assemblies", CanonicalArray.Of(Assemblies.Select(ToCanonicalFile))),
            ("assets", CanonicalArray.Of(Assets.Select(ToCanonicalFile))),
            ("component_id", CanonicalJson.Text(ComponentId)),
            ("component_version", CanonicalJson.Text(Version.ToString())),
            ("contract_range", range),
            ("display_name", CanonicalJson.Text(DisplayName)),
            ("entry_assembly", CanonicalJson.Text(EntryAssembly)),
            ("key_id", CanonicalJson.Text(KeyId)),
            ("publisher_id", CanonicalJson.Text(PublisherId)),
            ("signed_at", CanonicalJson.Number(SignedAt)));
    }

    /// <summary>The canonical JSON text the signature covers, without the domain prefix.</summary>
    public string CanonicalBody() => CanonicalJson.Write(ToCanonical());

    /// <summary>The exact bytes the publisher's Ed25519 signature covers.</summary>
    public byte[] SignedBytes() => CanonicalJson.Encode(CanonicalJson.ComponentManifestDomain, ToCanonical());

    /// <summary>The SHA-256 of <see cref="SignedBytes"/>, lower-case hex: the package's identity in a revocation list.</summary>
    public string SignedBytesSha256() => Ed25519.Sha256Hex(SignedBytes());

    /// <summary>
    /// Parses a <c>component.json</c> document, reporting every problem instead of throwing. A
    /// manifest is closed: an unknown member is refused, so a typo cannot silently drop a rule.
    /// </summary>
    /// <param name="json">The manifest text.</param>
    /// <param name="manifest">The parsed manifest, when it is sound.</param>
    /// <param name="problems">What was wrong; empty when the manifest parsed.</param>
    /// <returns>True when the manifest is sound.</returns>
    public static bool TryParse(string? json, out ComponentManifest? manifest, out IReadOnlyList<string> problems)
    {
        manifest = null;
        var found = new List<string>();
        if (string.IsNullOrWhiteSpace(json))
        {
            problems = new[] { "manifest is empty." };
            return false;
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json);
        }
        catch (JsonException error)
        {
            problems = new[] { "manifest is not JSON: " + error.Message };
            return false;
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                problems = new[] { "manifest root is an object." };
                return false;
            }

            RejectUnknown(root, found);
            var componentId = ReadString(root, "component_id", found);
            var displayName = ReadString(root, "display_name", found);
            var publisherId = ReadString(root, "publisher_id", found);
            var keyId = ReadString(root, "key_id", found);
            var entryAssembly = ReadString(root, "entry_assembly", found);
            var versionText = ReadString(root, "component_version", found);
            var signedAt = ReadInteger(root, "signed_at", found);
            var range = ReadRange(root, found);
            var assemblies = ReadFiles(root, "assemblies", found);
            var assets = ReadFiles(root, "assets", found);

            if (componentId is not null && !IdPattern().IsMatch(componentId))
            {
                found.Add("manifest.component_id: '" + componentId + "' is a dotted lowercase id such as 'acme.rating'.");
            }

            if (publisherId is not null && !PublisherPattern().IsMatch(publisherId))
            {
                found.Add("manifest.publisher_id: '" + publisherId + "' is a lowercase id such as 'acme'.");
            }

            if (displayName is not null && (displayName.Length == 0 || displayName.Length > 64))
            {
                found.Add("manifest.display_name: DisplayName is 1 to 64 characters.");
            }

            if (keyId is not null && !Ed25519.IsKeyId(keyId))
            {
                found.Add("manifest.key_id: KeyId is 32 lower-case hex characters.");
            }

            ComponentVersion version = default;
            if (versionText is null || !ComponentVersion.TryParse(versionText, out version))
            {
                found.Add("manifest.component_version: Version is written 'major.minor.patch'.");
            }

            if (assemblies.Count == 0)
            {
                found.Add("manifest.assemblies: a package ships at least one assembly.");
            }

            if (entryAssembly is not null && assemblies.All(a => !string.Equals(a.Path, entryAssembly, StringComparison.Ordinal)))
            {
                found.Add("manifest.entry_assembly: '" + ComponentRefusalText.SafePath(entryAssembly) + "' is not one of the listed assemblies.");
            }

            var all = assemblies.Concat(assets).ToArray();
            foreach (var duplicate in all.GroupBy(static f => f.Path, StringComparer.Ordinal).Where(static g => g.Count() > 1))
            {
                found.Add("manifest.files: '" + ComponentRefusalText.SafePath(duplicate.Key) + "' is listed twice.");
            }

            problems = found;
            if (found.Count > 0
                || componentId is null || displayName is null || publisherId is null || keyId is null
                || entryAssembly is null || versionText is null || signedAt is null || range is null)
            {
                return false;
            }

            manifest = new ComponentManifest(
                componentId,
                displayName,
                version,
                publisherId,
                range,
                entryAssembly,
                assemblies.OrderBy(static a => a.Path, StringComparer.Ordinal).ToArray(),
                assets.OrderBy(static a => a.Path, StringComparer.Ordinal).ToArray(),
                keyId,
                signedAt.Value);
            return true;
        }
    }

    private static readonly string[] KnownMembers =
    {
        "assemblies", "assets", "component_id", "component_version", "contract_range",
        "display_name", "entry_assembly", "key_id", "publisher_id", "signed_at",
    };

    private static void RejectUnknown(JsonElement root, List<string> problems)
    {
        foreach (var member in root.EnumerateObject())
        {
            if (!KnownMembers.Contains(member.Name, StringComparer.Ordinal))
            {
                problems.Add("manifest." + ComponentRefusalText.SafePath(member.Name) + ": the manifest has no such member.");
            }
        }

        foreach (var required in KnownMembers)
        {
            if (!root.TryGetProperty(required, out _))
            {
                problems.Add("manifest." + required + ": the manifest requires '" + required + "'.");
            }
        }
    }

    private static string? ReadString(JsonElement root, string name, List<string> problems)
    {
        if (!root.TryGetProperty(name, out var element))
        {
            return null;
        }

        if (element.ValueKind != JsonValueKind.String || string.IsNullOrEmpty(element.GetString()))
        {
            problems.Add("manifest." + name + ": '" + name + "' is a non-empty string.");
            return null;
        }

        return element.GetString();
    }

    private static long? ReadInteger(JsonElement root, string name, List<string> problems)
    {
        if (!root.TryGetProperty(name, out var element))
        {
            return null;
        }

        if (element.ValueKind != JsonValueKind.Number || !element.TryGetInt64(out var value) || value < 0)
        {
            problems.Add("manifest." + name + ": '" + name + "' is a non-negative integer (UTC seconds).");
            return null;
        }

        return value;
    }

    private static ContractVersionRange? ReadRange(JsonElement root, List<string> problems)
    {
        if (!root.TryGetProperty("contract_range", out var element) || element.ValueKind != JsonValueKind.Object)
        {
            problems.Add("manifest.contract_range: 'contract_range' is an object with minimum and maximum_exclusive.");
            return null;
        }

        var minimum = element.TryGetProperty("minimum", out var min) && min.ValueKind == JsonValueKind.String
            ? min.GetString()
            : null;
        var maximum = element.TryGetProperty("maximum_exclusive", out var max) && max.ValueKind == JsonValueKind.String
            ? max.GetString()
            : null;
        if (minimum is null || maximum is null
            || !ContractVersionRange.TryParse(">=" + minimum + " <" + maximum, out var range))
        {
            problems.Add("manifest.contract_range: the range covers exactly one major, as '>=1.0 <2.0' does.");
            return null;
        }

        foreach (var member in element.EnumerateObject())
        {
            if (member.Name is not ("minimum" or "maximum_exclusive"))
            {
                problems.Add("manifest.contract_range." + ComponentRefusalText.SafePath(member.Name) + ": no such member.");
            }
        }

        return range;
    }

    private static List<ManifestFile> ReadFiles(JsonElement root, string name, List<string> problems)
    {
        var files = new List<ManifestFile>();
        if (!root.TryGetProperty(name, out var array))
        {
            return files;
        }

        if (array.ValueKind != JsonValueKind.Array)
        {
            problems.Add("manifest." + name + ": '" + name + "' is an array of {path, sha256} objects.");
            return files;
        }

        foreach (var item in array.EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.Object
                || !item.TryGetProperty("path", out var pathElement) || pathElement.ValueKind != JsonValueKind.String
                || !item.TryGetProperty("sha256", out var hashElement) || hashElement.ValueKind != JsonValueKind.String)
            {
                problems.Add("manifest." + name + ": every entry is a {path, sha256} object.");
                continue;
            }

            var path = pathElement.GetString() ?? string.Empty;
            var hash = hashElement.GetString() ?? string.Empty;
            if (!ComponentPackagePaths.IsSafeRelativePath(path, out var pathProblem))
            {
                problems.Add("manifest." + name + ": " + pathProblem);
                continue;
            }

            if (hash.Length != 64 || !hash.All(static c => c is >= '0' and <= '9' or >= 'a' and <= 'f'))
            {
                problems.Add("manifest." + name + ": the sha256 of '" + ComponentRefusalText.SafePath(path) + "' is 64 lower-case hex characters.");
                continue;
            }

            foreach (var member in item.EnumerateObject())
            {
                if (member.Name is not ("path" or "sha256"))
                {
                    problems.Add("manifest." + name + "." + ComponentRefusalText.SafePath(member.Name) + ": no such member.");
                }
            }

            files.Add(new ManifestFile(path, hash));
        }

        return files;
    }

    /// <summary>
    /// The component id shape the T1 contract's <c>ComponentDescriptor</c> declares: a dotted
    /// reverse-DNS lower-case id. The manifest and the descriptor must agree on it, so the pattern is
    /// the same one and not a second opinion.
    /// </summary>
    [GeneratedRegex("^[a-z][a-z0-9_]*([.-][a-z][a-z0-9_]*)+$", RegexOptions.CultureInvariant)]
    private static partial Regex IdPattern();

    /// <summary>The publisher id shape: one lower-case segment, no dots.</summary>
    [GeneratedRegex("^[a-z][a-z0-9_]*$", RegexOptions.CultureInvariant)]
    private static partial Regex PublisherPattern();


    private static CanonicalValue ToCanonicalFile(ManifestFile file) =>
        CanonicalObject.Of(("path", CanonicalJson.Text(file.Path)), ("sha256", CanonicalJson.Text(file.Sha256)));

    /// <inheritdoc />
    public override string ToString() =>
        ComponentId + "@" + Version.ToString() + " published by " + PublisherId
        + " (contract " + ContractRange.ToString() + ", signed "
        + SignedAt.ToString(CultureInfo.InvariantCulture) + ")";
}
