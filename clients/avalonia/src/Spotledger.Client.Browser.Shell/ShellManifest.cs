using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Browser.Shell;

/// <summary>One published file: path (relative to the app root, '/' separators), size and SHA-256.</summary>
/// <param name="Path">The manifest path.</param>
/// <param name="Bytes">The size in bytes.</param>
/// <param name="Sha256">The content hash, 64 lower-case hex characters.</param>
public sealed record ShellEntry(string Path, long Bytes, string Sha256)
{
    /// <summary>The cache key: the path and the content hash. A changed file gets a new key.</summary>
    public string Key => KeyOf(Path, Sha256);

    /// <summary>
    /// True when the file is stored in the shell cache. The pre-compressed <c>.br</c> and <c>.gz</c> siblings are
    /// not: the browser decodes them, and the plain file's hash is what is checked.
    /// </summary>
    public bool IsCacheable => !Path.EndsWith(".br", StringComparison.Ordinal) && !Path.EndsWith(".gz", StringComparison.Ordinal);

    /// <summary>The cache key of <paramref name="path"/> with <paramref name="sha256"/>.</summary>
    public static string KeyOf(string path, string sha256) => path + "#" + sha256;

    /// <summary>The manifest path of a cache key (the part before the last '#').</summary>
    public static string PathOfKey(string key)
    {
        int cut = key.LastIndexOf('#');
        return cut < 0 ? key : key[..cut];
    }
}

/// <summary>
/// The parsed, validated <c>assets.json</c> of a build (see <c>docs/browser-publish.md</c>). Parsing fails
/// closed: a wrong version, a bad hash, an unsafe or duplicate path, an unsorted list or a build id that does
/// not match the files throws <see cref="ShellIntegrityException"/>, and nothing of such a manifest is used.
/// </summary>
public sealed class ShellManifest
{
    private readonly Dictionary<string, ShellEntry> _byPath;

    /// <summary>Creates a manifest from already-valid entries (tests and the installer use <see cref="Parse"/>).</summary>
    public ShellManifest(string build, IReadOnlyList<ShellEntry> entries)
    {
        Build = build;
        Entries = entries;
        _byPath = new Dictionary<string, ShellEntry>(entries.Count, StringComparer.Ordinal);
        foreach (var entry in entries)
        {
            _byPath[entry.Path] = entry;
        }
    }

    /// <summary>The build id (SHA-256 over every path and hash in path order).</summary>
    public string Build { get; }

    /// <summary>Every published file, sorted by path.</summary>
    public IReadOnlyList<ShellEntry> Entries { get; }

    /// <summary>The cacheable entries only.</summary>
    public IEnumerable<ShellEntry> CacheableEntries => Entries.Where(e => e.IsCacheable);

    /// <summary>The entry for <paramref name="path"/>, or null.</summary>
    public ShellEntry? Find(string path) => _byPath.GetValueOrDefault(path);

    /// <summary>True when <paramref name="value"/> is 64 lower-case hex characters.</summary>
    public static bool IsHash(string value)
    {
        if (value.Length != 64)
        {
            return false;
        }

        foreach (char c in value)
        {
            if (!(c is >= '0' and <= '9' or >= 'a' and <= 'f'))
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>
    /// True when <paramref name="path"/> is a plain relative path: no leading '/', no backslash, no '.' or
    /// '..' segment, no empty segment, no '?', '#' or ':' and no control character.
    /// </summary>
    public static bool IsSafePath(string path)
    {
        if (path.Length == 0 || path[0] == '/')
        {
            return false;
        }

        foreach (char c in path)
        {
            if (c < ' ' || c == '\\' || c == '?' || c == '#' || c == ':' || c == '\u007f')
            {
                return false;
            }
        }

        foreach (string segment in path.Split('/'))
        {
            if (segment.Length == 0 || segment == "." || segment == "..")
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>The build id for <paramref name="entries"/>: the same recipe as the publish step.</summary>
    public static string BuildIdOf(IReadOnlyList<ShellEntry> entries, IContentHasher hasher)
    {
        var text = new StringBuilder();
        foreach (var entry in entries)
        {
            text.Append(entry.Path).Append('\0').Append(entry.Sha256).Append('\n');
        }

        return hasher.Sha256Hex(Encoding.UTF8.GetBytes(text.ToString()));
    }

    /// <summary>Parses and validates <paramref name="json"/>.</summary>
    /// <exception cref="ShellIntegrityException">The manifest is not acceptable.</exception>
    public static ShellManifest Parse(string json, IContentHasher? hasher = null)
    {
        hasher ??= new Sha256ContentHasher();
        try
        {
            using var document = JsonDocument.Parse(json);
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object
                || !root.TryGetProperty("version", out var version) || version.ValueKind != JsonValueKind.Number
                || !version.TryGetInt32(out int versionNumber) || versionNumber != 1)
            {
                throw new ShellIntegrityException("The asset manifest has an unknown version.");
            }

            string? build = root.TryGetProperty("build", out var buildElement) && buildElement.ValueKind == JsonValueKind.String
                ? buildElement.GetString() : null;
            if (build is null || !IsHash(build))
            {
                throw new ShellIntegrityException("The asset manifest has no valid build id.");
            }

            if (!root.TryGetProperty("files", out var files) || files.ValueKind != JsonValueKind.Array)
            {
                throw new ShellIntegrityException("The asset manifest has no file list.");
            }

            var entries = new List<ShellEntry>();
            string? previous = null;
            foreach (var file in files.EnumerateArray())
            {
                var entry = ReadEntry(file);
                if (previous is not null && string.CompareOrdinal(previous, entry.Path) >= 0)
                {
                    throw new ShellIntegrityException("The asset manifest lists a path twice or out of order: " + entry.Path);
                }

                previous = entry.Path;
                entries.Add(entry);
            }

            if (!string.Equals(BuildIdOf(entries, hasher), build, StringComparison.Ordinal))
            {
                throw new ShellIntegrityException("The asset manifest's build id does not match its files.");
            }

            return new ShellManifest(build, entries);
        }
        catch (JsonException)
        {
            throw new ShellIntegrityException("The asset manifest is not valid JSON.");
        }
    }

    private static ShellEntry ReadEntry(JsonElement file)
    {
        if (file.ValueKind != JsonValueKind.Object
            || !file.TryGetProperty("path", out var path) || path.ValueKind != JsonValueKind.String
            || !file.TryGetProperty("sha256", out var hash) || hash.ValueKind != JsonValueKind.String
            || !file.TryGetProperty("bytes", out var bytes) || bytes.ValueKind != JsonValueKind.Number || !bytes.TryGetInt64(out long size) || size < 0)
        {
            throw new ShellIntegrityException("The asset manifest has a malformed file entry.");
        }

        string pathText = path.GetString() ?? string.Empty;
        string hashText = hash.GetString() ?? string.Empty;
        if (!IsSafePath(pathText))
        {
            throw new ShellIntegrityException("The asset manifest has an unsafe path: " + pathText.Replace('\n', ' '));
        }

        if (!IsHash(hashText))
        {
            throw new ShellIntegrityException("The asset manifest has a bad hash for " + pathText + ".");
        }

        return new ShellEntry(pathText, size, hashText);
    }

    /// <inheritdoc />
    public override string ToString() => string.Create(CultureInfo.InvariantCulture, $"build {Build[..12]}, {Entries.Count} files");
}
