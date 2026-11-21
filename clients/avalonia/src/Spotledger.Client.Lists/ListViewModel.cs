using System.Globalization;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Lists;

/// <summary>Where the list is in its life.</summary>
public enum ListPhase
{
    /// <summary>Metadata and rights are being read.</summary>
    Loading,

    /// <summary>The list works.</summary>
    Ready,

    /// <summary>The person has no read right on the DocType (or the summary does not mention it): nothing is queried.</summary>
    Denied,

    /// <summary>Metadata or rights could not be read: the list fails closed and shows the reason.</summary>
    Failed,
}

/// <summary>Where a CSV goes (a save dialog in the host; a list in tests).</summary>
public interface IListExportSink
{
    /// <summary>Saves the CSV text.</summary>
    Task SaveCsvAsync(string suggestedName, string csv);
}

/// <summary>Options of a list view model.</summary>
public sealed record ListViewModelOptions
{
    /// <summary>Culture for numbers and dates (default: the current culture).</summary>
    public CultureInfo? Culture { get; init; }

    /// <summary>Time zone for date-times (default: local).</summary>
    public TimeZoneInfo? Zone { get; init; }

    /// <summary>Source tuning.</summary>
    public ListSourceOptions? Source { get; init; }

    /// <summary>Quick search debounce.</summary>
    public TimeSpan SearchDebounce { get; init; } = TimeSpan.FromMilliseconds(250);

    /// <summary>The delay function (tests pass an immediate one).</summary>
    public Func<TimeSpan, CancellationToken, Task>? Delay { get; init; }

    /// <summary>Most documents one bulk delete takes.</summary>
    public int MaxBulkDelete { get; init; } = 100;

    /// <summary>Most rows a bulk export asks for.</summary>
    public int ExportRowLimit { get; init; } = 100_000;

    /// <summary>Where exports go; null disables the export action.</summary>
    public IListExportSink? ExportSink { get; init; }
}

/// <summary>The selected row, for a host that places list and form side by side (WP-UI-17).</summary>
/// <param name="DocType">The DocType.</param>
/// <param name="Id">The selected document id; null when none or several rows are selected.</param>
public sealed record ListRowSelection(string DocType, string? Id);

/// <summary>One document a bulk delete could not remove.</summary>
/// <param name="Id">The document id.</param>
/// <param name="Display">What the list showed for it.</param>
/// <param name="Error">The server's answer.</param>
public sealed record DeleteFailure(string Id, string Display, ApiError Error);

/// <summary>The outcome of a bulk delete.</summary>
/// <param name="Attempted">How many were tried.</param>
/// <param name="Deleted">How many went.</param>
/// <param name="Failures">The ones that did not.</param>
public sealed record BulkDeleteSummary(int Attempted, int Deleted, IReadOnlyList<DeleteFailure> Failures);

/// <summary>
/// The generated default list of any DocType. It reads the metadata and the permission summary first and
/// fails closed: no read right, no query. Columns come from the metadata (never a hidden field), values
/// come only from the query operation, and every filter and order goes to the server as structured data.
/// </summary>
public sealed partial class ListViewModel : ObservableObject, IDisposable
{
    private readonly IListGateway _gateway;
    private readonly IFormShell _shell;
    private readonly IFilterStore _store;
    private readonly string _user;
    private readonly ListViewModelOptions _options;
    private readonly ListCellFormatter _formatter;
    private readonly List<FilterCondition> _conditions = [];
    private readonly List<string> _selected = [];
    private CancellationTokenSource _searchCts = new();
    private CancellationTokenSource _lifetime = new();
    private DocTypeModel? _model;
    private DocTypePermissionState? _permissions;
    private IReadOnlyList<FieldModel> _readable = [];
    private IReadOnlyList<FieldModel> _defaults = [];
    private IReadOnlyList<FieldModel> _searchFields = [];
    private ListPreferences _preferences = ListPreferences.Empty;
    private ListPhase _phase = ListPhase.Loading;
    private string? _message;
    private string _searchText = string.Empty;
    private string _appliedSearch = string.Empty;
    private ListSource? _source;
    private ListColumnSet? _columnSet;
    private bool _busy;

    /// <summary>Creates the list for <paramref name="doctype"/>. <paramref name="user"/> keys the stored preferences.</summary>
    public ListViewModel(IListGateway gateway, IFormShell shell, IFilterStore store, string doctype, string user, ListViewModelOptions? options = null)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        DocType = ListLimits.IsIdentifier(doctype) ? doctype : throw new ArgumentException("Not a DocType name.", nameof(doctype));
        _user = user ?? string.Empty;
        _options = options ?? new ListViewModelOptions();
        _formatter = new ListCellFormatter(_options.Culture, _options.Zone);
        Title = doctype;
    }

    /// <summary>The DocType.</summary>
    public string DocType { get; }

    /// <summary>The list title (the DocType's label once known).</summary>
    public string Title { get; private set; }

    /// <summary>The phase.</summary>
    public ListPhase Phase
    {
        get => _phase;
        private set
        {
            if (SetField(ref _phase, value))
            {
                OnPropertyChanged(nameof(StatusText));
            }
        }
    }

    /// <summary>The reason shown when the list is denied or failed, or the last action's message.</summary>
    public string? Message
    {
        get => _message;
        private set
        {
            if (SetField(ref _message, value))
            {
                OnPropertyChanged(nameof(StatusText));
            }
        }
    }

    /// <summary>The person's rights on the DocType (none until read).</summary>
    public DocumentRights Rights { get; private set; } = DocumentRights.None;

    /// <summary>The source feeding the grid; null until ready.</summary>
    public ListSource? Source => _source;

    /// <summary>The current columns; null until ready.</summary>
    public ListColumnSet? Columns => _columnSet;

    /// <summary>The conditions of the filter builder.</summary>
    public IReadOnlyList<FilterCondition> Conditions => _conditions;

    /// <summary>The applied quick search text.</summary>
    public string AppliedSearch => _appliedSearch;

    /// <summary>The text in the search box (applied after the debounce).</summary>
    public string SearchText => _searchText;

    /// <summary>The saved filters of this person for this DocType.</summary>
    public IReadOnlyList<SavedFilter> SavedFilters => _preferences.SavedFilters;

    /// <summary>The selected document ids.</summary>
    public IReadOnlyList<string> SelectedIds => _selected;

    /// <summary>True while a bulk action runs.</summary>
    public bool IsBusy
    {
        get => _busy;
        private set => SetField(ref _busy, value);
    }

    /// <summary>The fields the filter builder offers: the view's standard filters first, then the rest.</summary>
    public IReadOnlyList<FieldModel> FilterFields => _readable
        .Where(f => FilterCondition.IsFilterable(f.Kind) && _permissions is not null && !_permissions.Masked.Contains(f.Name))
        .OrderByDescending(f => f.InStandardFilter)
        .ToArray();

    /// <summary>The fields the column chooser offers.</summary>
    public IReadOnlyList<FieldModel> ChooserFields => _readable;

    /// <summary>The fields the quick search looks in.</summary>
    public IReadOnlyList<FieldModel> SearchFields => _searchFields;

    /// <summary>True when the person may create documents.</summary>
    public bool CanCreate => Phase == ListPhase.Ready && Rights.Create && (_permissions?.CanCreate ?? false);

    /// <summary>True when the toolbar offers delete for the selection.</summary>
    public bool CanDelete => Phase == ListPhase.Ready && Rights.Delete && _selected.Count > 0 && !IsBusy;

    /// <summary>Raised when the selection narrows to one row (or to none).</summary>
    public event EventHandler<ListRowSelection>? SelectedRowChanged;

    /// <summary>Raised when the columns changed (the view republishes the grid's model).</summary>
    public event EventHandler? ColumnsChanged;

    /// <summary>Raised when the filter chips changed.</summary>
    public event EventHandler? FiltersChanged;

    /// <summary>Raised when the status line or the toolbar state changed.</summary>
    public event EventHandler? StateChanged;

    /// <summary>The status line: the filter description and the count, or the reason the list is not shown.</summary>
    public string StatusText
    {
        get
        {
            if (_phase != ListPhase.Ready)
            {
                return _message ?? (_phase == ListPhase.Loading ? "Loading…" : string.Empty);
            }

            var parts = new List<string>();
            var filter = DescribeFilter();
            parts.Add(filter.Length == 0 ? "No filter" : filter);
            parts.Add(CountText());
            if (_message is not null)
            {
                parts.Add(_message);
            }

            return string.Join(" · ", parts);
        }
    }

    /// <summary>The count text: exact, or "N+" while the server has more.</summary>
    public string CountText()
    {
        if (_source is null)
        {
            return string.Empty;
        }

        var culture = _options.Culture ?? CultureInfo.CurrentCulture;
        return _source.ExactTotal is { } total
            ? total == 1 ? "1 row" : total.ToString("N0", culture) + " rows"
            : _source.FetchedRows.ToString("N0", culture) + "+ rows";
    }

    /// <summary>The filter in words.</summary>
    public string DescribeFilter()
    {
        var parts = new List<string>();
        foreach (var condition in _conditions)
        {
            if (_model?.Field(condition.Field) is { } field)
            {
                parts.Add(condition.Describe(field));
            }
        }

        if (_appliedSearch.Length > 0)
        {
            parts.Add($"Search “{_appliedSearch}”");
        }

        return string.Join("; ", parts);
    }

    internal void RaiseState()
    {
        OnPropertyChanged(nameof(StatusText));
        OnPropertyChanged(nameof(CanDelete));
        OnPropertyChanged(nameof(CanCreate));
        StateChanged?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Reads metadata, rights and preferences, then opens the first query. Fails closed.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        Phase = ListPhase.Loading;
        try
        {
            var model = await _gateway.GetMetaAsync(DocType, cancellationToken).ConfigureAwait(true);
            var permission = await _gateway.GetPermissionsAsync(DocType, cancellationToken).ConfigureAwait(true);
            Rights = permission.Rights;
            _permissions = permission.Snapshot.DocTypes.TryGetValue(DocType, out var state) ? state : null;
            if (!Rights.Read || _permissions is null)
            {
                Message = "You do not have access to this list.";
                Phase = ListPhase.Denied;
                RaiseState();
                return;
            }

            _model = model;
            Title = string.IsNullOrWhiteSpace(model.Label) ? DocType : model.Label;
            OnPropertyChanged(nameof(Title));
            _readable = ListColumnPlanner.Readable(model, _permissions);
            _defaults = ListColumnPlanner.Defaults(_readable);
            _preferences = await _store.LoadAsync(_user, DocType, cancellationToken).ConfigureAwait(true);
            var layout = ListColumnPlanner.Layout(_readable, _defaults, _preferences.Columns);
            _columnSet = new ListColumnSet(ListColumnPlanner.Build(layout, _permissions), _formatter);
            _searchFields = ListColumnPlanner.SearchFields(_readable, layout.Select(l => l.Field).ToArray(), _permissions);
            _source = new ListSource(_gateway, DocType, _columnSet, null, null, _options.Source);
            _source.CountChanged += OnSourceChanged;
            _source.PageLoaded += OnSourceChanged;
            _source.QueryFailed += OnQueryFailed;
            Message = null;
            Phase = ListPhase.Ready;
            OnPropertyChanged(nameof(Source));
            OnPropertyChanged(nameof(Columns));
            ColumnsChanged?.Invoke(this, EventArgs.Empty);
            RaiseState();
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (ApiRequestException error)
        {
            Message = error.Error.Message;
            Phase = ListPhase.Failed;
            RaiseState();
        }
    }

    private void OnSourceChanged(object? sender, EventArgs e) => RaiseState();

    private void OnQueryFailed(object? sender, Exception error)
    {
        Message = error is ApiRequestException api ? api.Error.Message : "The list could not be loaded.";
        RaiseState();
    }

    /// <summary>Runs the query again (F5).</summary>
    public void Refresh()
    {
        if (_source is null)
        {
            return;
        }

        Message = null;
        _source.Refresh();
        RaiseState();
    }

    /// <summary>Opens a document in its own tab (Enter or double-click on a row).</summary>
    public void Open(string id)
    {
        if (Phase == ListPhase.Ready && !string.IsNullOrEmpty(id))
        {
            _shell.OpenDocument(DocType, id);
        }
    }

    /// <summary>Opens a new document (Ctrl+N); nothing happens without the create right.</summary>
    public bool New()
    {
        if (!CanCreate)
        {
            return false;
        }

        _shell.OpenDocument(DocType, null);
        return true;
    }

    /// <summary>The grid's selection changed.</summary>
    public void SetSelection(IReadOnlyList<string> ids)
    {
        _selected.Clear();
        _selected.AddRange(ids.Distinct(StringComparer.Ordinal));
        SelectedRowChanged?.Invoke(this, new ListRowSelection(DocType, _selected.Count == 1 ? _selected[0] : null));
        RaiseState();
    }

    /// <inheritdoc />
    public void Dispose()
    {
        _searchCts.Cancel();
        _searchCts.Dispose();
        _lifetime.Cancel();
        _lifetime.Dispose();
        if (_source is not null)
        {
            _source.CountChanged -= OnSourceChanged;
            _source.PageLoaded -= OnSourceChanged;
            _source.QueryFailed -= OnQueryFailed;
            _source.Dispose();
        }
    }
}
