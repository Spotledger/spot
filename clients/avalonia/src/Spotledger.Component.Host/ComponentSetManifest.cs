using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Spotledger.Component.Host;

/// <summary>
/// One component a browser shell was built with: the identity the signature covered and the two
/// digests a build can compare. <see cref="ManifestSha256"/> is the SHA-256 of the package's signed
/// manifest bytes — the digest a revocation list names and the digest the host's verifier already
/// established — and <see cref="EntryAssemblySha256"/> is the SHA-256 of the assembly file the shell
/// bundles, so the bytes in the shell can be compared with the bytes the publisher signed.
/// </summary>
/// <param name="ComponentId">The dotted component id.</param>
/// <param name="ComponentVersion">The component's own version, as SemVer text.</param>
/// <param name="PublisherId">The publisher the certificate names.</param>
/// <param name="KeyId">The signing key id, 32 lower-case hex characters.</param>
/// <param name="EntryAssemblySha256">The SHA-256 of the entry assembly's bytes, lower-case hex.</param>
/// <param name="ManifestSha256">The SHA-256 of the signed manifest bytes, lower-case hex.</param>
public sealed record ComponentSetEntry(
    string ComponentId,
    string ComponentVersion,
    string PublisherId,
    string KeyId,
    string EntryAssemblySha256,
    string ManifestSha256);

/// <summary>
/// The content-hashed manifest of a browser shell's component set: what
/// <c>tools/component-shell/build-shell.py</c> writes at build time and what the shell reads at
/// start-up to prove the component list it loaded is the list that was built.
/// <para>
/// The file is named <c>component-set-&lt;hash&gt;.json</c>, where <c>hash</c> is the first 16
/// lower-case hex characters of the SHA-256 of the file's own bytes. The name is therefore a
/// content address: a shell that loads a manifest whose bytes do not hash to the name it was found
/// under has a manifest nobody built. The document itself is closed (an unknown member is refused)
/// and lists its components sorted by id, so the same set produces the same bytes.
/// </para>
/// <para>
/// This is a <em>build</em> check, not a trust decision: the signature over each package's manifest
/// is what says the code may run. The set manifest says which packages the shell was built with.
/// </para>
/// </summary>
public sealed class ComponentSetManifest
{
    /// <summary>The only schema version this host writes and reads.</summary>
    public const int SchemaVersion = 1;

    /// <summary>The file name prefix of a content-hashed set manifest.</summary>
    public const string FileNamePrefix = "component-set-";

    /// <summary>The file name suffix of a content-hashed set manifest.</summary>
    public const string FileNameSuffix = ".json";

    /// <summary>The characters of the content hash the file name carries.</summary>
    public const int FileNameHashCharacters = 16;

    private static readonly string[] Members =
    {
        "components", "schema", "shell",
    };

    private static readonly string[] EntryMembers =
    {
        "component_id", "component_version", "entry_assembly_sha256", "key_id",
        "manifest_sha256", "publisher_id",
    };

    private ComponentSetManifest(string shell, IReadOnlyList<ComponentSetEntry> components, string contentSha256)
    {
        Shell = shell;
        Components = components;
        ContentSha256 = contentSha256;
    }

    /// <summary>The shell's name, as the build set it.</summary>
    public string Shell { get; }

    /// <summary>The components, sorted by id.</summary>
    public IReadOnlyList<ComponentSetEntry> Components { get; }

    /// <summary>The SHA-256 of the manifest's own bytes, lower-case hex.</summary>
    public string ContentSha256 { get; }

    /// <summary>The file name this manifest must have: its content hash is part of its name.</summary>
    public string FileName => "component-set-" + HashPrefix(ContentSha256) + FileNameSuffix;

    /// <summary>The content hash a file name carries: its first 16 characters.</summary>
    /// <param name="contentSha256">A lower-case hex SHA-256.</param>
    public static string HashPrefix(string contentSha256)
    {
        ArgumentException.ThrowIfNullOrEmpty(contentSha256);
        return contentSha256.Length <= FileNameHashCharacters
            ? contentSha256
            : contentSha256.Substring(0, FileNameHashCharacters);
    }

    /// <summary>
    /// Parses a set manifest and records the digest of the bytes it was parsed from, so
    /// <see cref="MatchesFileName"/> can check the content address.
    /// </summary>
    /// <param name="json">The manifest text.</param>
    /// <param name="manifest">The parsed manifest, when it is sound.</param>
    /// <param name="problems">What was wrong; empty when it parsed.</param>
    public static bool TryParse(string? json, out ComponentSetManifest? manifest, out IReadOnlyList<string> problems)
    {
        manifest = null;
        var found = new List<string>();
        if (string.IsNullOrWhiteSpace(json))
        {
            problems = new[] { "component set manifest is empty." };
            return false;
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json);
        }
        catch (JsonException error)
        {
            problems = new[] { "component set manifest is not JSON: " + error.Message };
            return false;
        }

        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                problems = new[] { "component set manifest is a JSON object." };
                return false;
            }

            foreach (var member in root.EnumerateObject())
            {
                if (!Members.Contains(member.Name, StringComparer.Ordinal))
                {
                    found.Add("component set manifest." + member.Name + ": no such member.");
                }
            }

            var schema = root.TryGetProperty("schema", out var schemaElement)
                && schemaElement.ValueKind == JsonValueKind.Number
                && schemaElement.TryGetInt32(out var schemaValue)
                    ? schemaValue
                    : -1;
            if (schema != SchemaVersion)
            {
                found.Add("component set manifest.schema: schema is " + SchemaVersion.ToString(CultureInfo.InvariantCulture) + ".");
            }

            var shell = root.TryGetProperty("shell", out var shellElement)
                && shellElement.ValueKind == JsonValueKind.String
                    ? shellElement.GetString()
                    : null;
            if (string.IsNullOrWhiteSpace(shell))
            {
                found.Add("component set manifest.shell: shell is a non-empty string.");
            }

            var components = new List<ComponentSetEntry>();
            if (!root.TryGetProperty("components", out var array) || array.ValueKind != JsonValueKind.Array)
            {
                found.Add("component set manifest.components: components is an array.");
            }
            else
            {
                foreach (var item in array.EnumerateArray())
                {
                    var entry = ReadEntry(item, found);
                    if (entry is not null)
                    {
                        components.Add(entry);
                    }
                }
            }

            if (found.Count > 0 || shell is null)
            {
                problems = found;
                return false;
            }

            var ordered = components.OrderBy(static c => c.ComponentId, StringComparer.Ordinal).ToArray();
            if (!ordered.SequenceEqual(components))
            {
                found.Add("component set manifest.components: the entries are sorted by component_id.");
            }

            foreach (var duplicate in components.GroupBy(static c => c.ComponentId, StringComparer.Ordinal).Where(static g => g.Count() > 1))
            {
                found.Add("component set manifest.components: '" + duplicate.Key + "' is listed twice.");
            }

            if (found.Count > 0)
            {
                problems = found;
                return false;
            }

            problems = Array.Empty<string>();
            manifest = new ComponentSetManifest(shell, components, Ed25519.Sha256Hex(Encoding.UTF8.GetBytes(json)));
            return true;
        }
    }

    /// <summary>
    /// True when <paramref name="path"/>'s file name carries this manifest's content hash, which is
    /// what makes the file a content-addressed record of one build.
    /// </summary>
    /// <param name="path">The path the manifest was read from.</param>
    public bool MatchesFileName(string path)
    {
        ArgumentException.ThrowIfNullOrEmpty(path);
        return string.Equals(Path.GetFileName(path), FileName, StringComparison.Ordinal);
    }

    /// <summary>
    /// Compares what the shell loaded with what the build listed. Every listed component must be
    /// installed at the listed version and with the listed signed-manifest digest, and nothing may be
    /// installed that the build did not list: a shell that loads a component its manifest does not
    /// name was not built with it.
    /// </summary>
    /// <param name="catalogue">The components the shell actually installed.</param>
    public IReadOnlyList<ComponentRefusal> Compare(ComponentCatalogue catalogue)
    {
        ArgumentNullException.ThrowIfNull(catalogue);
        var refusals = new List<ComponentRefusal>();
        foreach (var listed in Components)
        {
            if (!catalogue.TryResolve(listed.ComponentId, out var installed) || installed is null)
            {
                refusals.Add(new ComponentRefusal(
                    ComponentRefusalCodes.SetComponentMissing,
                    "'" + listed.ComponentId + "' is listed in " + FileName + " but is not installed."));
                continue;
            }

            var version = installed.Descriptor.Version.ToString();
            if (!string.Equals(version, listed.ComponentVersion, StringComparison.Ordinal))
            {
                refusals.Add(new ComponentRefusal(
                    ComponentRefusalCodes.SetVersionMismatch,
                    "'" + listed.ComponentId + "' is installed at " + version + " but " + FileName
                    + " was built with " + listed.ComponentVersion + "."));
            }

            if (!string.Equals(installed.Verified.SignedBytesSha256, listed.ManifestSha256, StringComparison.Ordinal))
            {
                refusals.Add(new ComponentRefusal(
                    ComponentRefusalCodes.SetDigestMismatch,
                    "'" + listed.ComponentId + "' was signed as " + installed.Verified.SignedBytesSha256
                    + " but " + FileName + " lists " + listed.ManifestSha256 + "."));
            }
        }

        foreach (var installed in catalogue.Components)
        {
            if (!Components.Any(c => string.Equals(c.ComponentId, installed.ComponentId, StringComparison.Ordinal)))
            {
                refusals.Add(new ComponentRefusal(
                    ComponentRefusalCodes.SetComponentUnexpected,
                    "'" + installed.ComponentId + "' is installed but " + FileName
                    + " does not list it: this shell was not built with that component."));
            }
        }

        return refusals;
    }

    /// <inheritdoc />
    public override string ToString() =>
        Shell + " with " + Components.Count.ToString(CultureInfo.InvariantCulture) + " component(s), content "
        + HashPrefix(ContentSha256);

    private static ComponentSetEntry? ReadEntry(JsonElement item, List<string> found)
    {
        if (item.ValueKind != JsonValueKind.Object)
        {
            found.Add("component set manifest.components: every entry is an object.");
            return null;
        }

        foreach (var member in item.EnumerateObject())
        {
            if (!EntryMembers.Contains(member.Name, StringComparer.Ordinal))
            {
                found.Add("component set manifest.components." + member.Name + ": no such member.");
            }
        }

        var componentId = ReadText(item, "component_id", found);
        var componentVersion = ReadText(item, "component_version", found);
        var publisherId = ReadText(item, "publisher_id", found);
        var keyId = ReadText(item, "key_id", found);
        var entryAssembly = ReadHex(item, "entry_assembly_sha256", found);
        var manifestSha = ReadHex(item, "manifest_sha256", found);
        if (componentId is null || componentVersion is null || publisherId is null || keyId is null
            || entryAssembly is null || manifestSha is null)
        {
            return null;
        }

        if (!Ed25519.IsKeyId(keyId))
        {
            found.Add("component set manifest.components." + componentId + ".key_id: key_id is 32 lower-case hex characters.");
            return null;
        }

        return new ComponentSetEntry(componentId, componentVersion, publisherId, keyId, entryAssembly, manifestSha);
    }

    private static string? ReadText(JsonElement item, string name, List<string> found)
    {
        if (item.TryGetProperty(name, out var element)
            && element.ValueKind == JsonValueKind.String
            && !string.IsNullOrEmpty(element.GetString()))
        {
            return element.GetString();
        }

        found.Add("component set manifest.components." + name + ": '" + name + "' is a non-empty string.");
        return null;
    }

    private static string? ReadHex(JsonElement item, string name, List<string> found)
    {
        if (item.TryGetProperty(name, out var element)
            && element.ValueKind == JsonValueKind.String
            && element.GetString() is { Length: 64 } text
            && text.All(static c => c is >= '0' and <= '9' or >= 'a' and <= 'f'))
        {
            return text;
        }

        found.Add("component set manifest.components." + name + ": '" + name + "' is 64 lower-case hex characters.");
        return null;
    }
}

/// <summary>
/// Reads a shell's set manifest from its published folder, refusing a file whose name does not
/// carry its own content hash. The check is one pass and needs no state, so the browser host can run
/// it before it installs anything.
/// </summary>
public static class ComponentSetManifestReader
{
    /// <summary>Reads and checks the set manifest at <paramref name="path"/>.</summary>
    /// <param name="path">The manifest file.</param>
    /// <param name="manifest">The manifest, when the file is sound.</param>
    /// <param name="refusal">Why it is not, when it is not.</param>
    public static bool TryRead(string path, out ComponentSetManifest? manifest, out ComponentRefusal? refusal)
    {
        manifest = null;
        refusal = null;
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        if (!File.Exists(path))
        {
            refusal = new ComponentRefusal(
                ComponentRefusalCodes.SetManifestMissing,
                "the component set manifest '" + path + "' does not exist: the shell was not built with one.");
            return false;
        }

        var text = File.ReadAllText(path);
        if (!ComponentSetManifest.TryParse(text, out manifest, out var problems) || manifest is null)
        {
            refusal = new ComponentRefusal(ComponentRefusalCodes.SetManifestInvalid, string.Join(" ", problems));
            return false;
        }

        if (!manifest.MatchesFileName(path))
        {
            refusal = new ComponentRefusal(
                ComponentRefusalCodes.SetNameMismatch,
                "the manifest at '" + Path.GetFileName(path) + "' hashes to " + manifest.ContentSha256
                + ", so its content-addressed name is " + manifest.FileName + ": the file was edited after the build.");
            manifest = null;
            return false;
        }

        return true;
    }
}
