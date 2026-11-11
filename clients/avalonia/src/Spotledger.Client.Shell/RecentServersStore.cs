using System.Text.Json;

namespace Spotledger.Client.Shell;

/// <summary>
/// Remembers recent server URLs in a JSON file under the per-user app data folder
/// (<c>Environment.SpecialFolder.ApplicationData</c>/Spotledger). Tests inject a
/// different folder; nothing else is persisted.
/// </summary>
public sealed class RecentServersStore
{
    private const int MaxRecent = 8;
    private static readonly JsonSerializerOptions FileOptions = new() { WriteIndented = true };

    private readonly string _filePath;

    public RecentServersStore()
        : this(Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
            "Spotledger",
            "recent-servers.json"))
    {
    }

    public RecentServersStore(string filePath)
    {
        _filePath = filePath ?? throw new ArgumentNullException(nameof(filePath));
    }

    /// <summary>The remembered URLs, most recent first. A missing or corrupt file reads as empty.</summary>
    public IReadOnlyList<string> Load()
    {
        try
        {
            if (!File.Exists(_filePath))
            {
                return Array.Empty<string>();
            }

            var urls = JsonSerializer.Deserialize<List<string>>(File.ReadAllText(_filePath));
            return urls is null ? Array.Empty<string>() : urls.Where(url => !string.IsNullOrWhiteSpace(url)).ToList();
        }
        catch (Exception exception) when (exception is IOException or JsonException or UnauthorizedAccessException)
        {
            return Array.Empty<string>();
        }
    }

    /// <summary>Puts <paramref name="url"/> at the front of the list and saves it.</summary>
    public void Remember(string url)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(url);
        var recent = new List<string> { url.TrimEnd('/') };
        recent.AddRange(Load().Where(existing => !string.Equals(existing.TrimEnd('/'), recent[0], StringComparison.OrdinalIgnoreCase)));
        Save(recent.Take(MaxRecent).ToList());
    }

    private void Save(List<string> urls)
    {
        var directory = Path.GetDirectoryName(_filePath);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }

        File.WriteAllText(_filePath, JsonSerializer.Serialize(urls, FileOptions));
    }
}
