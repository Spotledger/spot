using System.Text;

namespace Spotledger.Client.Browser.Shell.Tests;

/// <summary>An in-memory <see cref="IShellStorage"/>.</summary>
public sealed class FakeStorage : IShellStorage
{
    public Dictionary<string, byte[]> Blobs { get; } = new(StringComparer.Ordinal);

    public Dictionary<string, string> Records { get; } = new(StringComparer.Ordinal);

    public Task<byte[]?> GetAsync(string key, CancellationToken cancellationToken = default) =>
        Task.FromResult(Blobs.GetValueOrDefault(key));

    public Task PutAsync(string key, byte[] bytes, CancellationToken cancellationToken = default)
    {
        Blobs[key] = bytes;
        return Task.CompletedTask;
    }

    public Task DeleteAsync(string key, CancellationToken cancellationToken = default)
    {
        Blobs.Remove(key);
        return Task.CompletedTask;
    }

    public Task<IReadOnlyList<string>> ListKeysAsync(string prefix, CancellationToken cancellationToken = default) =>
        Task.FromResult<IReadOnlyList<string>>(Blobs.Keys.Where(k => k.StartsWith(prefix, StringComparison.Ordinal)).OrderBy(k => k, StringComparer.Ordinal).ToList());

    public Task<string?> ReadRecordAsync(string name, CancellationToken cancellationToken = default) =>
        Task.FromResult(Records.GetValueOrDefault(name));

    public Task WriteRecordAsync(string name, string value, CancellationToken cancellationToken = default)
    {
        Records[name] = value;
        return Task.CompletedTask;
    }

    public Task DeleteRecordAsync(string name, CancellationToken cancellationToken = default)
    {
        Records.Remove(name);
        return Task.CompletedTask;
    }

    public IReadOnlyList<string> ShellKeys() =>
        Blobs.Keys.Where(k => k.StartsWith(ShellInstaller.KeyPrefix, StringComparison.Ordinal)).Select(k => k[ShellInstaller.KeyPrefix.Length..]).OrderBy(k => k, StringComparer.Ordinal).ToList();
}

/// <summary>A fake server: serves a set of files, counts downloads, can go offline or serve wrong bytes.</summary>
public sealed class FakeNetwork : IShellNetwork
{
    public Dictionary<string, byte[]> Files { get; set; } = new(StringComparer.Ordinal);

    public List<string> Downloads { get; } = [];

    public bool Offline { get; set; }

    public HashSet<string> Corrupt { get; } = new(StringComparer.Ordinal);

    public Task<byte[]> FetchAsync(string path, CancellationToken cancellationToken = default)
    {
        if (Offline)
        {
            throw new ShellNetworkException("offline");
        }

        Downloads.Add(path);
        if (!Files.TryGetValue(path, out var bytes))
        {
            throw new ShellNetworkException("not found: " + path);
        }

        return Task.FromResult(Corrupt.Contains(path) ? [.. bytes, (byte)0] : bytes);
    }
}

/// <summary>A published build made from text files: the files, their manifest and the manifest JSON.</summary>
public sealed class FakeBuild
{
    private static readonly Sha256ContentHasher Hasher = new();

    public FakeBuild(IReadOnlyDictionary<string, string> files)
    {
        Files = files.ToDictionary(f => f.Key, f => Encoding.UTF8.GetBytes(f.Value), StringComparer.Ordinal);
        var entries = Files.OrderBy(f => f.Key, StringComparer.Ordinal)
            .Select(f => new ShellEntry(f.Key, f.Value.Length, Hasher.Sha256Hex(f.Value))).ToList();
        string build = ShellManifest.BuildIdOf(entries, Hasher);
        Manifest = new ShellManifest(build, entries);
        Json = ToJson(build, entries);
    }

    public Dictionary<string, byte[]> Files { get; }

    public ShellManifest Manifest { get; }

    public string Json { get; }

    public FakeBuild With(string path, string? content)
    {
        var next = Files.ToDictionary(f => f.Key, f => Encoding.UTF8.GetString(f.Value), StringComparer.Ordinal);
        if (content is null)
        {
            next.Remove(path);
        }
        else
        {
            next[path] = content;
        }

        return new FakeBuild(next);
    }

    public static FakeBuild Sample() => new(new Dictionary<string, string>
    {
        ["index.html"] = "<html>shell</html>",
        ["main.js"] = "start();",
        ["_framework/dotnet.wasm"] = "wasm-bytes",
        ["_framework/dotnet.wasm.br"] = "br-bytes",
        ["_framework/Spotledger.Client.Forms.dll"] = "forms",
    });

    public static string ToJson(string build, IEnumerable<ShellEntry> entries)
    {
        var files = string.Join(",", entries.Select(e => $"{{\"path\":\"{e.Path}\",\"bytes\":{e.Bytes},\"sha256\":\"{e.Sha256}\"}}"));
        return $"{{\"version\":1,\"build\":\"{build}\",\"files\":[{files}]}}";
    }
}
