using System.Globalization;
using System.Text.Json;

namespace Spotledger.Client.Workspace.Split;

/// <summary>How the list and the form share the screen.</summary>
public enum SplitMode
{
    /// <summary>The list alone; the form pane is folded away.</summary>
    List,

    /// <summary>The list beside the form of the selected row.</summary>
    Split,

    /// <summary>The form alone; the list is folded away (walking it still works).</summary>
    Form,
}

/// <summary>
/// What the arrangement remembers for one DocType: the mode and the splitter position. The position is
/// a whole percentage of the width given to the list, never a float, so the same preference renders the
/// same at every window size and stores exactly.
/// </summary>
/// <param name="Mode">The mode last used.</param>
/// <param name="ListPercent">The list's share of the workspace, 20..80.</param>
public sealed record SplitPreference(SplitMode Mode, int ListPercent)
{
    /// <summary>The narrowest the list may be made.</summary>
    public const int MinListPercent = 20;

    /// <summary>The widest the list may be made.</summary>
    public const int MaxListPercent = 80;

    /// <summary>The first-run arrangement: the list beside the form, the list a little narrower.</summary>
    public static SplitPreference Default { get; } = new(SplitMode.Split, 40);

    /// <summary>The same preference in another mode.</summary>
    public SplitPreference WithMode(SplitMode mode) => this with { Mode = mode };

    /// <summary>The same preference with the list's share clamped into range.</summary>
    public SplitPreference WithListPercent(int percent)
        => this with { ListPercent = Math.Clamp(percent, MinListPercent, MaxListPercent) };
}

/// <summary>
/// Where the arrangement's preferences live, per user and per DocType (the brief: "remembered per
/// DocType per user"). A missing entry means the DocType was never opened in the arrangement.
/// </summary>
public interface ISplitPreferenceStore
{
    /// <summary>Loads the preference, or null when there is none.</summary>
    SplitPreference? Load(string user, string docType);

    /// <summary>Stores the preference for one DocType.</summary>
    void Save(string user, string docType, SplitPreference preference);
}

/// <summary>Preferences kept for the life of the process (tests, and a host with no settings file yet).</summary>
public sealed class InMemorySplitPreferenceStore : ISplitPreferenceStore
{
    private readonly Dictionary<(string User, string DocType), SplitPreference> _values = [];

    /// <inheritdoc />
    public SplitPreference? Load(string user, string docType)
        => _values.TryGetValue((user ?? string.Empty, docType ?? string.Empty), out var value) ? value : null;

    /// <inheritdoc />
    public void Save(string user, string docType, SplitPreference preference)
    {
        ArgumentNullException.ThrowIfNull(preference);
        _values[(user ?? string.Empty, docType ?? string.Empty)] = preference;
    }
}

/// <summary>
/// Preferences in one small JSON file in the user's settings folder: a map from user and DocType to the
/// mode and the splitter share. A file that cannot be read is treated as empty (the arrangement opens
/// in its default mode) rather than failing the screen, and a write is atomic so a crash mid-save
/// cannot leave half a file behind.
/// </summary>
public sealed class JsonSplitPreferenceStore : ISplitPreferenceStore
{
    private static readonly JsonSerializerOptions JsonOptions = new() { WriteIndented = true };

    private readonly string _path;
    private readonly Lock _gate = new();
    private Dictionary<string, Stored> _map = [];

    /// <summary>Opens (or creates on first save) the file at <paramref name="path"/>.</summary>
    public JsonSplitPreferenceStore(string path)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        _path = path;
        _map = Read();
    }

    /// <inheritdoc />
    public SplitPreference? Load(string user, string docType)
    {
        lock (_gate)
        {
            return _map.TryGetValue(Key(user, docType), out var stored)
                ? new SplitPreference(stored.Mode, Math.Clamp(stored.ListPercent, SplitPreference.MinListPercent, SplitPreference.MaxListPercent))
                : null;
        }
    }

    /// <inheritdoc />
    public void Save(string user, string docType, SplitPreference preference)
    {
        ArgumentNullException.ThrowIfNull(preference);
        lock (_gate)
        {
            _map[Key(user, docType)] = new Stored(preference.Mode, preference.ListPercent);
            var directory = Path.GetDirectoryName(_path);
            if (!string.IsNullOrEmpty(directory))
            {
                _ = Directory.CreateDirectory(directory);
            }

            var temporary = _path + ".tmp";
            File.WriteAllText(temporary, JsonSerializer.Serialize(_map, JsonOptions));
            File.Move(temporary, _path, overwrite: true);
        }
    }

    private static string Key(string user, string docType)
        => (user ?? string.Empty).ToLowerInvariant() + "|" + (docType ?? string.Empty).ToLowerInvariant();

    private Dictionary<string, Stored> Read()
    {
        try
        {
            if (!File.Exists(_path))
            {
                return [];
            }

            return JsonSerializer.Deserialize<Dictionary<string, Stored>>(File.ReadAllText(_path), JsonOptions) ?? [];
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return [];
        }
    }

    /// <summary>One stored row; the JSON is the shape the host writes, not a contract with a server.</summary>
    private sealed record Stored(SplitMode Mode, int ListPercent)
    {
        /// <summary>The stored share, clamped when it is read back from an older or edited file.</summary>
        public int ListPercent { get; init; } = Math.Clamp(ListPercent, SplitPreference.MinListPercent, SplitPreference.MaxListPercent);

        /// <inheritdoc />
        public override string ToString() => string.Create(CultureInfo.InvariantCulture, $"{Mode} {ListPercent}%");
    }
}
