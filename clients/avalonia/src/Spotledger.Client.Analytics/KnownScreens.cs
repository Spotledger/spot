namespace Spotledger.Client.Analytics;

/// <summary>
/// The screen ids analytics is allowed to report: the union of the screens manifest
/// (tools/ui-coverage/screens/*.toml). <see cref="UsageAnalytics"/> records nothing for an id that
/// is not in this set, so a bug - or a hostile view model - cannot smuggle a document name into a
/// screen id. An empty set rejects everything: there is no permissive mode.
/// </summary>
public interface IKnownScreens
{
    /// <summary>True only for an id listed in the manifest.</summary>
    bool Contains(string? screenId);

    /// <summary>How many ids the set holds (0 for <see cref="KnownScreens.Empty"/>).</summary>
    int Count { get; }
}

/// <summary>
/// An immutable set of known screen ids. <see cref="Manifest"/> is the list the build generated
/// into <c>KnownScreens.g.cs</c> from the screens manifest, so the shipped client reads no file at
/// runtime. The empty set is the correct value while the manifest holds no real screens: analytics
/// then record nothing rather than everything.
/// </summary>
public sealed partial class KnownScreens : IKnownScreens
{
    private readonly HashSet<string> _ids;

    private KnownScreens(IEnumerable<string> ids)
    {
        _ids = new HashSet<string>(ids, StringComparer.Ordinal);
    }

    /// <summary>Rejects every id (the fail-closed default).</summary>
    public static KnownScreens Empty { get; } = new(Array.Empty<string>());

    /// <summary>The ids generated from tools/ui-coverage/screens/*.toml at build time.</summary>
    public static KnownScreens Manifest { get; } = new(ManifestIds());

    /// <summary>A set for the Shell's own tests and for callers that do not use the manifest.</summary>
    public static KnownScreens From(IEnumerable<string> ids)
    {
        ArgumentNullException.ThrowIfNull(ids);
        var list = new List<string>();
        foreach (var id in ids)
        {
            if (!AnalyticsIds.IsValidId(id))
            {
                throw new ArgumentException("Every known screen id must match the analytics id pattern.", nameof(ids));
            }

            list.Add(id);
        }

        return new KnownScreens(list);
    }

    public int Count => _ids.Count;

    /// <summary>The ids in ordinal order; for diagnostics and the manifest test, never for a payload.</summary>
    public IReadOnlyList<string> Ids
    {
        get
        {
            var list = new List<string>(_ids);
            list.Sort(StringComparer.Ordinal);
            return list;
        }
    }

    public bool Contains(string? screenId) => !string.IsNullOrEmpty(screenId) && _ids.Contains(screenId);
}
