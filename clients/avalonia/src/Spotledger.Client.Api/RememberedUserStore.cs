using System.Text.Json;

namespace Spotledger.Client.Api;

/// <summary>
/// The non-secret companion to the token store: which user last chose "Remember me" on
/// which server, so a start-up resume knows where to look in the keyring
/// (<c>&lt;server&gt;/&lt;user&gt;</c>). A user name is not a credential; only the token
/// itself is secret. JSON file under the per-user app data folder, written only when
/// "Remember me" was used.
/// </summary>
public sealed class RememberedUserStore
{
    private static readonly JsonSerializerOptions FileOptions = new() { WriteIndented = true };

    private readonly string _filePath;

    public RememberedUserStore()
        : this(Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
            "Spotledger",
            "remembered-users.json"))
    {
    }

    public RememberedUserStore(string filePath)
    {
        _filePath = filePath ?? throw new ArgumentNullException(nameof(filePath));
    }

    /// <summary>The user last remembered for <paramref name="serverUrl"/> (authority match), or null.</summary>
    public string? Load(string serverUrl)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(serverUrl);
        foreach (var (server, user) in ReadFile())
        {
            if (string.Equals(server, Normalize(serverUrl), StringComparison.OrdinalIgnoreCase))
            {
                return user;
            }
        }

        return null;
    }

    /// <summary>Records the user for whom a token was stored under "Remember me".</summary>
    public void Remember(string serverUrl, string user)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(serverUrl);
        ArgumentException.ThrowIfNullOrWhiteSpace(user);
        var entries = ReadFile()
            .Where(entry => !string.Equals(entry.Server, Normalize(serverUrl), StringComparison.OrdinalIgnoreCase))
            .ToList();
        entries.Add(new Entry(Normalize(serverUrl), user.Trim()));
        var directory = Path.GetDirectoryName(_filePath);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }

        File.WriteAllText(_filePath, JsonSerializer.Serialize(entries, FileOptions));
    }

    /// <summary>Forgets the hint (sign-out of a remembered session deletes the token too).</summary>
    public void Forget(string serverUrl)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(serverUrl);
        var remaining = ReadFile()
            .Where(entry => !string.Equals(entry.Server, Normalize(serverUrl), StringComparison.OrdinalIgnoreCase))
            .ToList();
        var directory = Path.GetDirectoryName(_filePath);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }

        File.WriteAllText(_filePath, JsonSerializer.Serialize(remaining, FileOptions));
    }

    private List<Entry> ReadFile()
    {
        try
        {
            if (!File.Exists(_filePath))
            {
                return [];
            }

            return JsonSerializer.Deserialize<List<Entry>>(File.ReadAllText(_filePath)) ?? [];
        }
        catch (Exception exception) when (exception is IOException or JsonException or UnauthorizedAccessException)
        {
            // A missing or corrupt hint file costs exactly one conventional sign-in.
            return [];
        }
    }

    private static string Normalize(string serverUrl) =>
        Uri.TryCreate(serverUrl, UriKind.Absolute, out var uri)
            ? uri.GetLeftPart(UriPartial.Authority)
            : serverUrl.Trim().TrimEnd('/');

    private sealed record Entry(string Server, string User);
}