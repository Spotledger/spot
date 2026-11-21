using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Notifications;

/// <summary>What is remembered locally: which notice ids were read, and two switches. Never a notice body.</summary>
/// <param name="ReadIds">Ids of read notices, oldest first, at most <see cref="NoticeStateStore.MaxReadIds"/>.</param>
/// <param name="DoNotDisturb">Toasts are off; the bell still counts.</param>
/// <param name="AutoReloadClean">A clean form reloads by itself when someone else changes the document.</param>
public sealed record NoticeState(IReadOnlyList<string> ReadIds, bool DoNotDisturb, bool AutoReloadClean)
{
    /// <summary>Nothing read, both switches off.</summary>
    public static NoticeState Empty { get; } = new([], false, false);
}

/// <summary>Where notice state is kept, per user and per server.</summary>
public interface INoticeStateStore
{
    /// <summary>Loads the state; empty when none was saved or it cannot be read.</summary>
    NoticeState Load(string server, string user);

    /// <summary>Saves the state; a failure keeps it in memory for this run and never throws.</summary>
    void Save(string server, string user, NoticeState state);
}

/// <summary>Constants shared by the stores.</summary>
public static class NoticeStateStore
{
    /// <summary>The most read ids kept: the retained window of the feed.</summary>
    public const int MaxReadIds = NotificationFeed.RetainedNotices;
}

/// <summary>State for this run only (tests, and the fallback when a file cannot be written).</summary>
public sealed class InMemoryNoticeStateStore : INoticeStateStore
{
    private readonly Dictionary<string, NoticeState> _states = new(StringComparer.Ordinal);

    /// <inheritdoc/>
    public NoticeState Load(string server, string user)
    {
        lock (_states)
        {
            return _states.GetValueOrDefault(server + "\n" + user) ?? NoticeState.Empty;
        }
    }

    /// <inheritdoc/>
    public void Save(string server, string user, NoticeState state)
    {
        lock (_states)
        {
            _states[server + "\n" + user] = state;
        }
    }
}

/// <summary>
/// One small JSON file per user and server under the local data folder (the draft store's convention:
/// a hashed name, a temp file renamed over the old one). Ids and flags only.
/// </summary>
public sealed class FileNoticeStateStore(string root) : INoticeStateStore
{
    private readonly InMemoryNoticeStateStore _memory = new();

    /// <summary>The default root: the user's local data folder.</summary>
    public static string DefaultRoot() =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Spotledger", "notifications");

    /// <inheritdoc/>
    public NoticeState Load(string server, string user)
    {
        var path = PathFor(server, user);
        try
        {
            if (File.Exists(path))
            {
                using var document = JsonDocument.Parse(File.ReadAllBytes(path));
                var rootElement = document.RootElement;
                var ids = new List<string>();
                if (rootElement.TryGetProperty("read", out var read) && read.ValueKind == JsonValueKind.Array)
                {
                    ids.AddRange(read.EnumerateArray().Where(e => e.ValueKind == JsonValueKind.String).Select(e => e.GetString()!));
                }

                return new NoticeState(
                    ids.TakeLast(NoticeStateStore.MaxReadIds).ToList(),
                    Flag(rootElement, "dnd"),
                    Flag(rootElement, "auto_reload_clean"));
            }
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or JsonException)
        {
            // A damaged or unreadable file is the same as no file.
        }

        return _memory.Load(server, user);
    }

    /// <inheritdoc/>
    public void Save(string server, string user, NoticeState state)
    {
        ArgumentNullException.ThrowIfNull(state);
        _memory.Save(server, user, state);
        var path = PathFor(server, user);
        try
        {
            Directory.CreateDirectory(root);
            var temp = path + ".tmp";
            using (var stream = File.Create(temp))
            using (var writer = new Utf8JsonWriter(stream))
            {
                writer.WriteStartObject();
                writer.WriteStartArray("read");
                foreach (var id in state.ReadIds.TakeLast(NoticeStateStore.MaxReadIds))
                {
                    writer.WriteStringValue(id);
                }

                writer.WriteEndArray();
                writer.WriteBoolean("dnd", state.DoNotDisturb);
                writer.WriteBoolean("auto_reload_clean", state.AutoReloadClean);
                writer.WriteEndObject();
            }

            File.Move(temp, path, overwrite: true);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // Kept in memory for this run.
        }
    }

    private static bool Flag(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.True;

    private string PathFor(string server, string user)
    {
        var hash = SHA256.HashData(Encoding.UTF8.GetBytes(server + "\n" + user));
        return Path.Combine(root, Convert.ToHexString(hash.AsSpan(0, 16)).ToLowerInvariant() + ".json");
    }
}
