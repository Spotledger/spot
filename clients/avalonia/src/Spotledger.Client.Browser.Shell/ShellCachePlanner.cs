using System.Text.Json;

namespace Spotledger.Client.Browser.Shell;

/// <summary>
/// The version record of the shell cache: which build is active, which contract version it was built for and
/// which cache keys it needs. It is written last, so a half-finished download never looks like an installed build.
/// </summary>
/// <param name="Build">The active build id.</param>
/// <param name="ContractVersion">The client contract version the build speaks.</param>
/// <param name="Keys">The cache keys the build needs, sorted ordinally.</param>
public sealed record ShellCacheRecord(string Build, string ContractVersion, IReadOnlyList<string> Keys)
{
    /// <summary>The record as JSON.</summary>
    public string ToJson()
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteString("build", Build);
            writer.WriteString("contract", ContractVersion);
            writer.WriteStartArray("keys");
            foreach (string key in Keys)
            {
                writer.WriteStringValue(key);
            }

            writer.WriteEndArray();
            writer.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>Reads a record; null when the text is missing or not a valid record (treated as a cold start).</summary>
    public static ShellCacheRecord? TryParse(string? json)
    {
        if (string.IsNullOrEmpty(json))
        {
            return null;
        }

        try
        {
            using var document = JsonDocument.Parse(json);
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object
                || !root.TryGetProperty("build", out var build) || build.ValueKind != JsonValueKind.String
                || !root.TryGetProperty("contract", out var contract) || contract.ValueKind != JsonValueKind.String
                || !root.TryGetProperty("keys", out var keys) || keys.ValueKind != JsonValueKind.Array)
            {
                return null;
            }

            var list = new List<string>();
            foreach (var key in keys.EnumerateArray())
            {
                if (key.ValueKind != JsonValueKind.String)
                {
                    return null;
                }

                list.Add(key.GetString() ?? string.Empty);
            }

            return new ShellCacheRecord(build.GetString() ?? string.Empty, contract.GetString() ?? string.Empty, list);
        }
        catch (JsonException)
        {
            return null;
        }
    }
}

/// <summary>Why the planner decided what it did.</summary>
public enum ShellPlanReason
{
    /// <summary>Nothing is installed: download everything.</summary>
    ColdStart,

    /// <summary>The contract version changed: the cache is cleared and everything is downloaded again.</summary>
    ContractChanged,

    /// <summary>A new build: download the files whose hash is new, keep the rest.</summary>
    Update,

    /// <summary>Same build, but some files are gone from the cache (evicted): download only those.</summary>
    Repair,

    /// <summary>Same build and everything present: download nothing.</summary>
    Unchanged,
}

/// <summary>What to do to move the cache to a new build. Fetch, Keep and Delete are cache keys, sorted ordinally.</summary>
/// <param name="Reason">Why.</param>
/// <param name="Fetch">The entries to download.</param>
/// <param name="Keep">The keys already present and still needed.</param>
/// <param name="Delete">The keys present and not needed by the new build (removed only when the new build activates).</param>
/// <param name="ClearAll">True when the contract version changed or nothing was installed.</param>
public sealed record ShellCachePlan(
    ShellPlanReason Reason,
    IReadOnlyList<ShellEntry> Fetch,
    IReadOnlyList<string> Keep,
    IReadOnlyList<string> Delete,
    bool ClearAll);

/// <summary>
/// The shell cache planner: given the version record and the keys actually present, and the manifest of the new
/// build, decide which entries to fetch, keep and delete. A changed hash is a new key, so it replaces; an
/// unchanged hash is kept. It never lists for deletion a key the new build needs. Pure: no clock, no storage.
/// </summary>
public static class ShellCachePlanner
{
    /// <summary>Plans the move to <paramref name="next"/>.</summary>
    /// <param name="record">The active version record, or null when none (or an unreadable one).</param>
    /// <param name="presentKeys">The shell keys actually in the cache (the browser may evict).</param>
    /// <param name="next">The manifest of the new build.</param>
    /// <param name="nextContractVersion">The contract version the new build speaks.</param>
    public static ShellCachePlan Plan(
        ShellCacheRecord? record,
        IReadOnlyCollection<string> presentKeys,
        ShellManifest next,
        string nextContractVersion)
    {
        var needed = next.CacheableEntries.ToList();
        var neededKeys = new HashSet<string>(needed.Select(e => e.Key), StringComparer.Ordinal);
        var present = new HashSet<string>(presentKeys, StringComparer.Ordinal);

        var delete = present.Where(k => !neededKeys.Contains(k)).OrderBy(k => k, StringComparer.Ordinal).ToList();

        if (record is null)
        {
            return new ShellCachePlan(ShellPlanReason.ColdStart, Sorted(needed), [], delete, ClearAll: true);
        }

        if (!string.Equals(record.ContractVersion, nextContractVersion, StringComparison.Ordinal))
        {
            return new ShellCachePlan(ShellPlanReason.ContractChanged, Sorted(needed), [], delete, ClearAll: true);
        }

        var fetch = needed.Where(e => !present.Contains(e.Key)).ToList();
        var keep = neededKeys.Where(present.Contains).OrderBy(k => k, StringComparer.Ordinal).ToList();
        var reason = string.Equals(record.Build, next.Build, StringComparison.Ordinal)
            ? (fetch.Count == 0 ? ShellPlanReason.Unchanged : ShellPlanReason.Repair)
            : ShellPlanReason.Update;
        return new ShellCachePlan(reason, Sorted(fetch), keep, delete, ClearAll: false);
    }

    private static List<ShellEntry> Sorted(List<ShellEntry> entries) =>
        entries.OrderBy(e => e.Key, StringComparer.Ordinal).ToList();
}
