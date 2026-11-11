using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Spotledger.Client.Lists;

/// <summary>A column of the person's layout.</summary>
/// <param name="Field">The field name.</param>
/// <param name="Width">The pixel width the person set, if any.</param>
public sealed record ColumnPreference(string Field, double? Width = null);

/// <summary>A named filter the person saved.</summary>
/// <param name="Name">The name.</param>
/// <param name="Conditions">The builder conditions (all must hold).</param>
/// <param name="Search">The quick search text.</param>
public sealed record SavedFilter(string Name, IReadOnlyList<FilterCondition> Conditions, string? Search = null);

/// <summary>What the list remembers per person per DocType: the visible columns in order with
/// their widths (null: the view's defaults) and the saved filters.</summary>
public sealed record ListPreferences(IReadOnlyList<ColumnPreference>? Columns, IReadOnlyList<SavedFilter> SavedFilters)
{
    /// <summary>Nothing remembered.</summary>
    public static ListPreferences Empty { get; } = new(null, []);

    /// <summary>The most bytes a stored file may have.</summary>
    public const int MaxBytes = 256 * 1024;

    private static readonly JsonSerializerOptions Options = new()
    {
        Converters = { new JsonStringEnumConverter() },
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        MaxDepth = 8,
    };

    /// <summary>Serialises the preferences.</summary>
    public string ToJson() => JsonSerializer.Serialize(Normalize(this), Options);

    /// <summary>Reads preferences; anything malformed or oversized gives <see cref="Empty"/>.</summary>
    public static ListPreferences FromJson(string? json)
    {
        if (string.IsNullOrWhiteSpace(json) || json.Length > MaxBytes)
        {
            return Empty;
        }

        try
        {
            return Normalize(JsonSerializer.Deserialize<ListPreferences>(json, Options) ?? Empty);
        }
        catch (Exception error) when (error is JsonException or NotSupportedException or ArgumentException)
        {
            return Empty;
        }
    }

    /// <summary>Caps counts and drops entries that are not valid identifiers.</summary>
    public static ListPreferences Normalize(ListPreferences value)
    {
        var columns = value.Columns?
            .Where(c => c is not null && ListLimits.IsIdentifier(c.Field))
            .Select(c => new ColumnPreference(c.Field, c.Width is { } w && double.IsFinite(w) ? Math.Clamp(w, 24, 2000) : null))
            .DistinctBy(c => c.Field)
            .Take(64)
            .ToList();
        var saved = (value.SavedFilters ?? [])
            .Where(f => f is not null && !string.IsNullOrWhiteSpace(f.Name))
            .Select(f => new SavedFilter(
                f.Name.Trim()[..Math.Min(f.Name.Trim().Length, 80)],
                (f.Conditions ?? [])
                    .Where(c => c is not null && ListLimits.IsIdentifier(c.Field) && c.Values is not null && c.Values.Count <= ListLimits.MaxInValues)
                    .Select(c => c with { Values = c.Values.Select(v => v ?? string.Empty).Select(v => v[..Math.Min(v.Length, 1000)]).ToArray() })
                    .Take(ListLimits.MaxNodes - 2)
                    .ToArray(),
                f.Search is null ? null : f.Search[..Math.Min(f.Search.Length, 200)]))
            .DistinctBy(f => f.Name, StringComparer.Ordinal)
            .Take(50)
            .ToArray();
        return new ListPreferences(columns, saved);
    }
}

/// <summary>A store that keeps preferences in memory (tests, and a host without a settings
/// folder).</summary>
public sealed class InMemoryFilterStore : IFilterStore
{
    private readonly Dictionary<(string, string), ListPreferences> _items = [];

    /// <summary>How many saves happened.</summary>
    public int Saves { get; private set; }

    /// <inheritdoc />
    public Task<ListPreferences> LoadAsync(string user, string doctype, CancellationToken cancellationToken)
        => Task.FromResult(_items.TryGetValue((user, doctype), out var value) ? value : ListPreferences.Empty);

    /// <inheritdoc />
    public Task SaveAsync(string user, string doctype, ListPreferences preferences, CancellationToken cancellationToken)
    {
        _items[(user, doctype)] = ListPreferences.Normalize(preferences);
        Saves++;
        return Task.CompletedTask;
    }
}

/// <summary>Keeps preferences in the local settings folder, one file per person and DocType.
/// The file name is derived from a hash of the user key and the (validated) DocType name, so
/// neither can climb out of the folder. The server has no user-settings resource in the
/// contract (0.7.0), so this is where preferences live.</summary>
public sealed class FileFilterStore(string directory) : IFilterStore
{
    /// <inheritdoc />
    public async Task<ListPreferences> LoadAsync(string user, string doctype, CancellationToken cancellationToken)
    {
        var path = PathFor(user, doctype);
        try
        {
            var info = new FileInfo(path);
            if (!info.Exists || info.Length > ListPreferences.MaxBytes)
            {
                return ListPreferences.Empty;
            }

            return ListPreferences.FromJson(await File.ReadAllTextAsync(path, cancellationToken).ConfigureAwait(false));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            return ListPreferences.Empty;
        }
    }

    /// <inheritdoc />
    public async Task SaveAsync(string user, string doctype, ListPreferences preferences, CancellationToken cancellationToken)
    {
        var path = PathFor(user, doctype);
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
            var temp = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            await File.WriteAllTextAsync(temp, preferences.ToJson(), cancellationToken).ConfigureAwait(false);
            File.Move(temp, path, overwrite: true);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            // Preferences are a convenience: a store that cannot write loses them, nothing else.
        }
    }

    private string PathFor(string user, string doctype)
    {
        if (!ListLimits.IsIdentifier(doctype))
        {
            throw new ArgumentException("Not a DocType name.", nameof(doctype));
        }

        var hash = Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(user ?? string.Empty)))[..24].ToLowerInvariant();
        return Path.Combine(directory, hash, doctype + ".json");
    }
}
