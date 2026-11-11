using Spotledger.Client.Api;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Reports;

/// <summary>Where the catalogue is.</summary>
public enum CataloguePhase
{
    /// <summary>The list is being read.</summary>
    Loading,

    /// <summary>Reports are listed.</summary>
    Ready,

    /// <summary>The server answered and the person may see no report.</summary>
    Empty,

    /// <summary>The server could not be reached or refused the list.</summary>
    Unreachable,
}

/// <summary>One report in the catalogue.</summary>
/// <param name="Summary">The report.</param>
/// <param name="IsFavourite">True when the person starred it.</param>
/// <param name="IsRecent">True when the person opened it lately.</param>
public sealed record ReportEntry(ReportSummary Summary, bool IsFavourite, bool IsRecent);

/// <summary>The reports of one app, in display order.</summary>
/// <param name="App">The app.</param>
/// <param name="Entries">Recently opened reports first, then by label.</param>
public sealed record ReportGroup(string App, IReadOnlyList<ReportEntry> Entries);

/// <summary>
/// The Reports workspace tab: the reports the caller may see, searchable by label, grouped by app with the
/// recently opened ones first, with favourites remembered per person. It lists what the server returns and
/// decides no permission; an empty or unreachable catalogue says so in one line.
/// </summary>
public sealed class ReportCatalogueViewModel : ObservableObject
{
    private readonly IReportGateway _gateway;
    private readonly IReportShell _shell;
    private readonly IReportSettingsStore _store;
    private readonly string _user;
    private IReadOnlyList<ReportSummary> _all = [];
    private ReportUserSettings _settings = ReportUserSettings.Empty;
    private CataloguePhase _phase = CataloguePhase.Loading;
    private string _search = string.Empty;
    private bool _onlyFavourites;
    private string? _message;

    /// <summary>Creates the catalogue; <paramref name="user"/> keys the remembered favourites and recents.</summary>
    public ReportCatalogueViewModel(IReportGateway gateway, IReportShell shell, IReportSettingsStore store, string user)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _user = user ?? string.Empty;
    }

    /// <summary>Raised when the groups, phase or message changed.</summary>
    public event EventHandler? Changed;

    /// <summary>The phase.</summary>
    public CataloguePhase Phase
    {
        get => _phase;
        private set => SetField(ref _phase, value);
    }

    /// <summary>The one line that says why nothing is listed (empty or unreachable); null otherwise.</summary>
    public string? Message
    {
        get => _message;
        private set => SetField(ref _message, value);
    }

    /// <summary>The search text.</summary>
    public string SearchText => _search;

    /// <summary>True when only favourites are listed.</summary>
    public bool OnlyFavourites => _onlyFavourites;

    /// <summary>Every report the server listed, before search.</summary>
    public IReadOnlyList<ReportSummary> All => _all;

    /// <summary>The listed reports, grouped by app, after search and the favourites filter.</summary>
    public IReadOnlyList<ReportGroup> Groups { get; private set; } = [];

    /// <summary>How many reports are listed after search.</summary>
    public int ListedCount => Groups.Sum(g => g.Entries.Count);

    /// <summary>Reads the list and the person's favourites.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        Phase = CataloguePhase.Loading;
        try
        {
            _all = await _gateway.ListAsync(cancellationToken).ConfigureAwait(true);
        }
        catch (ApiRequestException error)
        {
            _all = [];
            Message = error.Error.Code == "api.unreachable" ? "The server cannot be reached, so the reports cannot be listed." : error.Error.Message;
            Phase = CataloguePhase.Unreachable;
            Rebuild();
            return;
        }

        _settings = await _store.LoadAsync(_user, cancellationToken).ConfigureAwait(true);
        Message = _all.Count == 0 ? "No reports are available to you." : null;
        Phase = _all.Count == 0 ? CataloguePhase.Empty : CataloguePhase.Ready;
        Rebuild();
    }

    /// <summary>Filters the list by label (or name), ignoring case.</summary>
    public void SetSearch(string? text)
    {
        _search = (text ?? string.Empty).Trim();
        OnPropertyChanged(nameof(SearchText));
        Rebuild();
    }

    /// <summary>Lists only the starred reports, or all.</summary>
    public void SetOnlyFavourites(bool only)
    {
        _onlyFavourites = only;
        OnPropertyChanged(nameof(OnlyFavourites));
        Rebuild();
    }

    /// <summary>Stars or unstars a report for this person.</summary>
    public async Task ToggleFavouriteAsync(string name, CancellationToken cancellationToken = default)
    {
        if (!ReportUserSettings.IsIdentifier(name))
        {
            return;
        }

        var current = await _store.LoadAsync(_user, cancellationToken).ConfigureAwait(true);
        var favourites = current.Favourites.Contains(name, StringComparer.Ordinal)
            ? current.Favourites.Where(f => f != name).ToArray()
            : current.Favourites.Append(name).ToArray();
        _settings = ReportUserSettings.Normalize(current with { Favourites = favourites });
        await _store.SaveAsync(_user, _settings, cancellationToken).ConfigureAwait(true);
        Rebuild();
    }

    /// <summary>Opens the report's tab (the shell focuses it when it is already open with no parameters).</summary>
    public bool Open(string name)
    {
        var report = _all.FirstOrDefault(r => r.Name == name);
        if (report is null)
        {
            return false;
        }

        _shell.OpenReport(report.Name, report.Label, null);
        return true;
    }

    private void Rebuild()
    {
        var favourites = _settings.Favourites.ToHashSet(StringComparer.Ordinal);
        var recent = _settings.Recent.Select((name, index) => (name, index)).ToDictionary(p => p.name, p => p.index, StringComparer.Ordinal);
        Groups = _all
            .Where(r => _search.Length == 0 || r.Label.Contains(_search, StringComparison.OrdinalIgnoreCase) || r.Name.Contains(_search, StringComparison.OrdinalIgnoreCase))
            .Where(r => !_onlyFavourites || favourites.Contains(r.Name))
            .GroupBy(r => r.App, StringComparer.Ordinal)
            .OrderBy(g => g.Key, StringComparer.OrdinalIgnoreCase)
            .Select(g => new ReportGroup(
                g.Key,
                g.OrderBy(r => recent.TryGetValue(r.Name, out var rank) ? rank : int.MaxValue)
                    .ThenBy(r => r.Label, StringComparer.OrdinalIgnoreCase)
                    .Select(r => new ReportEntry(r, favourites.Contains(r.Name), recent.ContainsKey(r.Name)))
                    .ToArray()))
            .ToArray();
        if (_phase == CataloguePhase.Ready)
        {
            Message = ListedCount == 0 ? "No report matches." : null;
        }

        OnPropertyChanged(nameof(Groups));
        OnPropertyChanged(nameof(ListedCount));
        Changed?.Invoke(this, EventArgs.Empty);
    }
}
