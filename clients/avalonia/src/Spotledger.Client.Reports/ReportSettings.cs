using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Spotledger.Client.Reports;

/// <summary>A named set of parameter values the person saved for one report.</summary>
/// <param name="Name">The name the person gave it.</param>
/// <param name="Values">Parameter name to the value as text (the same text the form shows and parses).</param>
public sealed record ReportParameterSet(string Name, IReadOnlyDictionary<string, string> Values);

/// <summary>What the client remembers per person: favourite and recent reports and the saved parameter sets of
/// every report.</summary>
/// <param name="Favourites">Report names the person starred.</param>
/// <param name="Recent">Report names, the most recently opened first.</param>
/// <param name="Sets">Report name to its saved parameter sets.</param>
public sealed record ReportUserSettings(
    IReadOnlyList<string> Favourites,
    IReadOnlyList<string> Recent,
    IReadOnlyDictionary<string, IReadOnlyList<ReportParameterSet>> Sets)
{
    /// <summary>The most bytes a stored file may have.</summary>
    public const int MaxBytes = 512 * 1024;

    /// <summary>Most favourites kept.</summary>
    public const int MaxFavourites = 200;

    /// <summary>Most recent reports kept.</summary>
    public const int MaxRecent = 30;

    /// <summary>Most reports with saved sets.</summary>
    public const int MaxSetReports = 200;

    /// <summary>Most saved sets per report.</summary>
    public const int MaxSetsPerReport = 50;

    /// <summary>Most values in one set.</summary>
    public const int MaxValuesPerSet = 64;

    private static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        MaxDepth = 8,
    };

    private static readonly Regex Identifier = new("^[A-Za-z][A-Za-z0-9_]{0,127}$", RegexOptions.CultureInvariant | RegexOptions.Compiled, TimeSpan.FromSeconds(1));

    /// <summary>Nothing remembered.</summary>
    public static ReportUserSettings Empty { get; } = new([], [], new Dictionary<string, IReadOnlyList<ReportParameterSet>>());

    /// <summary>True when <paramref name="name"/> can be a report or parameter name.</summary>
    public static bool IsIdentifier(string? name) => name is not null && Identifier.IsMatch(name);

    /// <summary>Serialises the settings.</summary>
    public string ToJson() => JsonSerializer.Serialize(Normalize(this), Json);

    /// <summary>Reads settings; anything malformed or oversized gives <see cref="Empty"/>.</summary>
    public static ReportUserSettings FromJson(string? json)
    {
        if (string.IsNullOrWhiteSpace(json) || json.Length > MaxBytes)
        {
            return Empty;
        }

        try
        {
            return Normalize(JsonSerializer.Deserialize<ReportUserSettings>(json, Json) ?? Empty);
        }
        catch (Exception error) when (error is JsonException or NotSupportedException or ArgumentException)
        {
            return Empty;
        }
    }

    /// <summary>Drops what is not a valid identifier or name and caps every count.</summary>
    public static ReportUserSettings Normalize(ReportUserSettings value)
    {
        var sets = new Dictionary<string, IReadOnlyList<ReportParameterSet>>(StringComparer.Ordinal);
        foreach (var (report, list) in value.Sets ?? new Dictionary<string, IReadOnlyList<ReportParameterSet>>())
        {
            if (!IsIdentifier(report) || list is null || sets.Count >= MaxSetReports)
            {
                continue;
            }

            var kept = list
                .Where(s => s is not null && !string.IsNullOrWhiteSpace(s.Name) && s.Values is not null)
                .Select(s => new ReportParameterSet(
                    Clip(s.Name.Trim(), 80),
                    s.Values.Where(p => IsIdentifier(p.Key) && p.Value is not null).Take(MaxValuesPerSet)
                        .ToDictionary(p => p.Key, p => Clip(p.Value, 1000), StringComparer.Ordinal)))
                .DistinctBy(s => s.Name, StringComparer.Ordinal)
                .Take(MaxSetsPerReport)
                .ToArray();
            if (kept.Length > 0)
            {
                sets[report] = kept;
            }
        }

        return new ReportUserSettings(
            (value.Favourites ?? []).Where(IsIdentifier).Distinct(StringComparer.Ordinal).Take(MaxFavourites).ToArray(),
            (value.Recent ?? []).Where(IsIdentifier).Distinct(StringComparer.Ordinal).Take(MaxRecent).ToArray(),
            sets);
    }

    private static string Clip(string text, int length) => text.Length <= length ? text : text[..length];
}

/// <summary>
/// Reads and writes what the client remembers about reports, per person. The contract (0.7.0) has no
/// user-settings resource, so the production store is local; a server-side store can implement this
/// interface later without changing any caller.
/// </summary>
public interface IReportSettingsStore
{
    /// <summary>Loads the person's settings; an unreadable or absent store gives <see cref="ReportUserSettings.Empty"/>.</summary>
    Task<ReportUserSettings> LoadAsync(string user, CancellationToken cancellationToken);

    /// <summary>Saves the person's settings. A store that cannot write loses them; nothing else is affected.</summary>
    Task SaveAsync(string user, ReportUserSettings settings, CancellationToken cancellationToken);
}

/// <summary>Keeps settings in memory (tests, and a host without a settings folder).</summary>
public sealed class InMemoryReportSettingsStore : IReportSettingsStore
{
    private readonly Dictionary<string, ReportUserSettings> _items = new(StringComparer.Ordinal);

    /// <summary>How many saves happened.</summary>
    public int Saves { get; private set; }

    /// <inheritdoc />
    public Task<ReportUserSettings> LoadAsync(string user, CancellationToken cancellationToken)
        => Task.FromResult(_items.TryGetValue(user ?? string.Empty, out var value) ? value : ReportUserSettings.Empty);

    /// <inheritdoc />
    public Task SaveAsync(string user, ReportUserSettings settings, CancellationToken cancellationToken)
    {
        _items[user ?? string.Empty] = ReportUserSettings.Normalize(settings);
        Saves++;
        return Task.CompletedTask;
    }
}

/// <summary>Keeps settings in the local settings folder, one file per person. The file name is a hash of the
/// user key, so it cannot climb out of the folder.</summary>
public sealed class FileReportSettingsStore(string directory) : IReportSettingsStore
{
    /// <inheritdoc />
    public async Task<ReportUserSettings> LoadAsync(string user, CancellationToken cancellationToken)
    {
        var path = PathFor(user);
        try
        {
            var info = new FileInfo(path);
            if (!info.Exists || info.Length > ReportUserSettings.MaxBytes)
            {
                return ReportUserSettings.Empty;
            }

            return ReportUserSettings.FromJson(await File.ReadAllTextAsync(path, cancellationToken).ConfigureAwait(false));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            return ReportUserSettings.Empty;
        }
    }

    /// <inheritdoc />
    public async Task SaveAsync(string user, ReportUserSettings settings, CancellationToken cancellationToken)
    {
        var path = PathFor(user);
        try
        {
            Directory.CreateDirectory(directory);
            var temp = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            await File.WriteAllTextAsync(temp, settings.ToJson(), cancellationToken).ConfigureAwait(false);
            File.Move(temp, path, overwrite: true);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            // Settings are a convenience: a store that cannot write loses them, nothing else.
        }
    }

    private string PathFor(string user)
    {
        var hash = Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(user ?? string.Empty)))[..24].ToLowerInvariant();
        return Path.Combine(directory, "reports-" + hash + ".json");
    }
}
