using System.Globalization;
using Spotledger.Client.Api;
using Spotledger.Client.Grid;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Reports;

/// <summary>Where a report tab is in its life.</summary>
public enum ReportPhase
{
    /// <summary>The description is being read.</summary>
    Loading,

    /// <summary>The form is ready; a run may start.</summary>
    Ready,

    /// <summary>A run is in flight.</summary>
    Running,

    /// <summary>The report cannot be shown (not found, or a parameter kind this client does not know). Nothing is run.</summary>
    Failed,
}

/// <summary>Options of a report view model.</summary>
public sealed record ReportViewModelOptions
{
    /// <summary>Culture for counts in messages (default: the current culture).</summary>
    public CultureInfo? Culture { get; init; }

    /// <summary>Where an export goes; null disables the export action.</summary>
    public IReportExportSink? ExportSink { get; init; }

    /// <summary>Where copied text goes; null disables copy.</summary>
    public IClipboardSink? Clipboard { get; init; }

    /// <summary>Resolves a link cell to its label (column name, id); null shows ids.</summary>
    public Func<string, string, string?>? LabelOf { get; init; }
}

/// <summary>
/// One report tab: the parameter form generated from the description, one run at a time, and the answer
/// shown honestly. It holds no report-specific rule. What it does not know it does not guess: a column the
/// server removed is not shown and not named, a cut result says so and has no total, an answer that no longer
/// belongs to the run that is current is dropped.
/// </summary>
public sealed class ReportViewModel : ObservableObject, IDisposable
{
    private readonly IReportGateway _gateway;
    private readonly IReportShell _shell;
    private readonly IReportSettingsStore _store;
    private readonly string _user;
    private readonly ReportViewModelOptions _options;
    private readonly CultureInfo _culture;
    private CancellationTokenSource _runCts = new();
    private Task? _runTask;
    private int _runId;
    private string _resultFilters = "{}";
    private ReportPhase _phase = ReportPhase.Loading;
    private string? _message;
    private ReportDescription? _description;
    private ReportParameters? _parameters;
    private ReportResultSource? _source;
    private bool _truncated;
    private int _removedColumns;
    private IReadOnlyList<string> _notes = [];
    private ReportUserSettings _settings = ReportUserSettings.Empty;
    private bool _disposed;

    /// <summary>Creates the tab model for <paramref name="report"/>; <paramref name="user"/> keys the remembered settings.</summary>
    public ReportViewModel(IReportGateway gateway, IReportShell shell, IReportSettingsStore store, string report, string user, ReportViewModelOptions? options = null)
    {
        _gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        _shell = shell ?? throw new ArgumentNullException(nameof(shell));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        ReportName = ReportUserSettings.IsIdentifier(report) ? report : throw new ArgumentException("Not a report name.", nameof(report));
        _user = user ?? string.Empty;
        _options = options ?? new ReportViewModelOptions();
        _culture = _options.Culture ?? CultureInfo.CurrentCulture;
        Title = report;
    }

    /// <summary>Raised when the phase, message, notes or counts changed.</summary>
    public event EventHandler? StateChanged;

    /// <summary>Raised when the form's values were set by the model (a saved set, a reset): the controls reload.</summary>
    public event EventHandler? ParametersChanged;

    /// <summary>Raised when a new result replaced the previous one (or it was cleared).</summary>
    public event EventHandler? ResultChanged;

    /// <summary>The report's name.</summary>
    public string ReportName { get; }

    /// <summary>The tab title (the report's label once known).</summary>
    public string Title { get; private set; }

    /// <summary>The phase.</summary>
    public ReportPhase Phase
    {
        get => _phase;
        private set
        {
            if (SetField(ref _phase, value))
            {
                OnPropertyChanged(nameof(StatusText));
                StateChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    /// <summary>The one-line message under the toolbar: an error, a refusal, or the outcome of the last action. Null when none.</summary>
    public string? Message
    {
        get => _message;
        private set
        {
            if (SetField(ref _message, value))
            {
                OnPropertyChanged(nameof(StatusText));
                StateChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    /// <summary>The description; null until read.</summary>
    public ReportDescription? Description => _description;

    /// <summary>The parameter form; null until the description is read.</summary>
    public ReportParameters? Parameters => _parameters;

    /// <summary>The grid source of the current result; null when nothing has run (or the run failed).</summary>
    public ReportResultSource? Source => _source;

    /// <summary>True when the server cut the result at the limit.</summary>
    public bool Truncated => _truncated;

    /// <summary>How many columns the server left out for this person (never which).</summary>
    public int RemovedColumns => _removedColumns;

    /// <summary>Other messages that came with the run, as plain text.</summary>
    public IReadOnlyList<string> Notes => _notes;

    /// <summary>The banner shown when the result is cut; null otherwise.</summary>
    public string? TruncationBanner => _truncated && _source is not null
        ? string.Create(_culture, $"Showing the first {_source.LoadedRows:N0} rows. The report has more: narrow the parameters to see the rest.")
        : null;

    /// <summary>The labels of the parameters that are still empty, which are the ones that can narrow a cut result.</summary>
    public IReadOnlyList<string> NarrowingParameters => _truncated && _parameters is not null
        ? _parameters.Items.Where(p => p.IsEmpty && p.Def.Type != ReportParameterType.Unknown).Select(p => p.Def.Label).ToArray()
        : [];

    /// <summary>The one-line note about removed columns; null when none were removed.</summary>
    public string? RemovedColumnsNote => _removedColumns switch
    {
        <= 0 => null,
        1 => "1 column is not shown because you may not see it.",
        var n => string.Create(_culture, $"{n} columns are not shown because you may not see them."),
    };

    /// <summary>The status line: what is happening, or how many rows are shown.</summary>
    public string StatusText
    {
        get
        {
            if (_phase == ReportPhase.Loading)
            {
                return "Loading the report…";
            }

            if (_phase == ReportPhase.Running)
            {
                return "Running…";
            }

            if (_phase == ReportPhase.Failed)
            {
                return _message ?? "The report cannot be shown.";
            }

            if (_source is null)
            {
                return _description is { Parameters.Count: > 0 } ? "Set the parameters and run the report." : string.Empty;
            }

            var loaded = _source.LoadedRows;
            var shown = _source.ShownRows;
            var rows = string.Create(_culture, $"{loaded:N0}") + (loaded == 1 ? " row" : " rows");
            return shown == loaded ? rows : string.Create(_culture, $"{shown:N0} of ") + rows;
        }
    }

    /// <summary>True when a run may start now.</summary>
    public bool CanRun => _phase == ReportPhase.Ready && _parameters is { HasUnsupported: false };

    /// <summary>True when the export action is available.</summary>
    public bool CanExport => _source is not null && _options.ExportSink is not null && _phase != ReportPhase.Running;

    /// <summary>The saved parameter sets of this report, in the order they were saved.</summary>
    public IReadOnlyList<ReportParameterSet> ParameterSets
        => _settings.Sets.TryGetValue(ReportName, out var sets) ? sets : [];

    /// <summary>The tab identity key of this report with the form's current values.</summary>
    public string TabKey => ReportTabs.KeyFor(ReportName, _parameters?.Digest() ?? ReportParameters.DigestOf(null));

    /// <summary>Reads the description and builds the form. A report with no parameters runs at once.
    /// <paramref name="initial"/> are parameter values as text the form starts with.</summary>
    public async Task LoadAsync(IReadOnlyDictionary<string, string>? initial = null, CancellationToken cancellationToken = default)
    {
        Phase = ReportPhase.Loading;
        try
        {
            _description = await _gateway.DescribeAsync(ReportName, cancellationToken).ConfigureAwait(true);
        }
        catch (ApiRequestException error)
        {
            Fail(error.Error.Code == "report.not_found" ? "This report is not available." : error.Error.Message);
            return;
        }

        Title = _description.Label;
        _parameters = new ReportParameters(_description.Parameters);
        if (_parameters.HasUnsupported)
        {
            Fail("This report uses a parameter this version of the client cannot show.");
            return;
        }

        if (initial is not null)
        {
            _parameters.Apply(initial);
        }

        _settings = await _store.LoadAsync(_user, cancellationToken).ConfigureAwait(true);
        await UpdateSettingsAsync(s => s with { Recent = new[] { ReportName }.Concat(s.Recent.Where(r => r != ReportName)).Take(ReportUserSettings.MaxRecent).ToArray() }, cancellationToken).ConfigureAwait(true);
        OnPropertyChanged(nameof(Title));
        OnPropertyChanged(nameof(Description));
        OnPropertyChanged(nameof(Parameters));
        OnPropertyChanged(nameof(ParameterSets));
        Phase = ReportPhase.Ready;
        ParametersChanged?.Invoke(this, EventArgs.Empty);
        if (_parameters.Items.Count == 0)
        {
            await RunAsync().ConfigureAwait(true);
        }
    }

    /// <summary>Validates the form and runs the report. A run already in flight is not repeated: the call joins it.</summary>
    public Task RunAsync()
    {
        if (_runTask is { IsCompleted: false })
        {
            return _runTask;
        }

        return _runTask = RunCoreAsync();
    }

    /// <summary>Stops waiting for the run in flight and ignores its answer.</summary>
    public void CancelRun()
    {
        if (_phase != ReportPhase.Running)
        {
            return;
        }

        _runId++;
        _runCts.Cancel();
        _runTask = null;
        Message = "The run was cancelled.";
        Phase = ReportPhase.Ready;
    }

    /// <summary>Sets the quick filter on the loaded rows.</summary>
    public void SetQuickFilter(string? text)
    {
        _source?.SetFilter(text);
        OnPropertyChanged(nameof(StatusText));
        StateChanged?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Returns the form to the defaults.</summary>
    public void ResetParameters()
    {
        _parameters?.ResetAll();
        ParametersChanged?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Writes the shown rows as CSV to the destination the sink opens. Uses the server's export operation
    /// when the report maps to one, otherwise writes the loaded rows here, a row at a time.</summary>
    public async Task<bool> ExportCsvAsync(CancellationToken cancellationToken = default)
    {
        if (!CanExport || _source is null || _description is null)
        {
            return false;
        }

        var stream = await _options.ExportSink!.OpenAsync(ReportName + ".csv", cancellationToken).ConfigureAwait(true);
        if (stream is null)
        {
            return false;
        }

        await using (stream.ConfigureAwait(true))
        {
            try
            {
                if (_gateway.CanExportOnServer(_description))
                {
                    await _gateway.ExportCsvAsync(_description, _resultFilters, stream, cancellationToken).ConfigureAwait(true);
                    Message = "Exported the report.";
                }
                else
                {
                    var count = await ReportCsv.WriteAsync(stream, _source.Columns, _source.ViewRows(), cancellationToken).ConfigureAwait(true);
                    Message = string.Create(_culture, $"Exported {count:N0} rows.");
                }
            }
            catch (ApiRequestException error)
            {
                Message = error.Error.Message;
                return false;
            }
            catch (IOException error)
            {
                Message = "The file could not be written: " + error.Message;
                return false;
            }
        }

        return true;
    }

    /// <summary>Copies the rows at <paramref name="viewRows"/> (grid positions) as tab-separated text.</summary>
    public async Task<string?> CopySelectionAsync(IReadOnlyList<int> viewRows, CancellationToken cancellationToken = default)
    {
        if (_source is null || viewRows.Count == 0)
        {
            return null;
        }

        var rows = viewRows.Where(r => r >= 0 && r < _source.ShownRows).Order().Select(_source.RawRow).ToArray();
        var text = ReportCsv.ToTsv(_source.Columns, rows);
        if (_options.Clipboard is { } clipboard)
        {
            await clipboard.SetTextAsync(text, cancellationToken).ConfigureAwait(true);
        }

        return text;
    }

    /// <summary>Saves the form's values under a name (asked for when <paramref name="name"/> is null). A set with the same name is replaced.</summary>
    public async Task<bool> SaveParameterSetAsync(string? name = null, CancellationToken cancellationToken = default)
    {
        if (_parameters is null)
        {
            return false;
        }

        name ??= await _shell.AskTextAsync("Save parameters", "Name for this set of parameters").ConfigureAwait(true);
        name = name?.Trim();
        if (string.IsNullOrEmpty(name))
        {
            return false;
        }

        var set = new ReportParameterSet(name[..Math.Min(name.Length, 80)], _parameters.Snapshot());
        await UpdateSettingsAsync(s =>
        {
            var sets = s.Sets.ToDictionary(p => p.Key, p => p.Value, StringComparer.Ordinal);
            List<ReportParameterSet> list = sets.TryGetValue(ReportName, out var existing) ? existing.ToList() : [];
            var at = list.FindIndex(x => x.Name == set.Name);
            if (at >= 0)
            {
                list[at] = set;
            }
            else
            {
                list.Add(set);
            }

            sets[ReportName] = list;
            return s with { Sets = sets };
        }, cancellationToken).ConfigureAwait(true);
        OnPropertyChanged(nameof(ParameterSets));
        Message = $"Saved the parameters as \"{set.Name}\".";
        return true;
    }

    /// <summary>Puts a saved set into the form. Values that no longer fit are left empty and reported.</summary>
    public bool ApplyParameterSet(string name)
    {
        var set = ParameterSets.FirstOrDefault(s => s.Name == name);
        if (set is null || _parameters is null)
        {
            return false;
        }

        var failed = _parameters.Apply(set.Values);
        Message = failed.Count == 0 ? null : "Some saved values no longer fit: " + string.Join(", ", failed.Select(n => _parameters.Find(n)?.Def.Label ?? n)) + ".";
        ParametersChanged?.Invoke(this, EventArgs.Empty);
        return true;
    }

    /// <summary>Deletes a saved set.</summary>
    public async Task<bool> DeleteParameterSetAsync(string name, CancellationToken cancellationToken = default)
    {
        if (ParameterSets.All(s => s.Name != name))
        {
            return false;
        }

        await UpdateSettingsAsync(s =>
        {
            var sets = s.Sets.ToDictionary(p => p.Key, p => p.Value, StringComparer.Ordinal);
            sets[ReportName] = sets[ReportName].Where(x => x.Name != name).ToArray();
            return s with { Sets = sets };
        }, cancellationToken).ConfigureAwait(true);
        OnPropertyChanged(nameof(ParameterSets));
        return true;
    }

    /// <inheritdoc />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _runId++;
        _runCts.Cancel();
        _runCts.Dispose();
    }

    private async Task RunCoreAsync()
    {
        if (!CanRun || _parameters is null || _description is null)
        {
            return;
        }

        if (!_parameters.Validate())
        {
            Message = "Fix the marked parameters, then run the report.";
            ParametersChanged?.Invoke(this, EventArgs.Empty);
            return;
        }

        var id = ++_runId;
        _runCts = new CancellationTokenSource();
        var token = _runCts.Token;
        var filters = _parameters.ToFiltersJson();
        Message = null;
        Phase = ReportPhase.Running;
        ReportRunResult result;
        try
        {
            result = await _gateway.RunAsync(ReportName, filters, _description.MaxRows, token).ConfigureAwait(true);
        }
        catch (OperationCanceledException)
        {
            return;
        }
        catch (ApiRequestException error)
        {
            if (id == _runId)
            {
                OnRunFailed(error.Error);
            }

            return;
        }

        if (id != _runId || _disposed)
        {
            // A run that was cancelled or replaced: its answer belongs to nobody.
            return;
        }

        Show(result, filters);
    }

    private void OnRunFailed(ApiError error)
    {
        ClearResult();
        if (error.Code == "report.not_found")
        {
            Fail("This report is not available.");
            return;
        }

        var mapped = error.Code == "report.bad_argument" && _parameters is not null && _parameters.MapServerError(error);
        Message = error.Message;
        Phase = ReportPhase.Ready;
        if (mapped)
        {
            ParametersChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    private void Show(ReportRunResult result, string filters)
    {
        var summable = _description!.Columns.Where(c => c.Summable).Select(c => c.Name).ToHashSet(StringComparer.Ordinal);
        var columns = result.Columns.Select(c => summable.Contains(c.Name) ? c with { Summable = true } : c).ToArray();
        _source = new ReportResultSource(columns, result.Rows, result.Truncated) { LabelOf = _options.LabelOf };
        _truncated = result.Truncated;
        _resultFilters = filters;
        var removed = result.Messages.Where(m => m.Code == ReportJson.ColumnsRemovedCode).ToArray();
        _removedColumns = removed.Length == 0 ? 0 : removed.Sum(CountIn);
        _notes = result.Messages.Where(m => m.Code != ReportJson.ColumnsRemovedCode && m.Text.Length > 0).Select(m => m.Text).ToArray();
        Message = null;
        Phase = ReportPhase.Ready;
        OnPropertyChanged(nameof(Source));
        OnPropertyChanged(nameof(Truncated));
        OnPropertyChanged(nameof(TruncationBanner));
        OnPropertyChanged(nameof(NarrowingParameters));
        OnPropertyChanged(nameof(RemovedColumnsNote));
        OnPropertyChanged(nameof(Notes));
        OnPropertyChanged(nameof(StatusText));
        OnPropertyChanged(nameof(CanExport));
        ResultChanged?.Invoke(this, EventArgs.Empty);
        StateChanged?.Invoke(this, EventArgs.Empty);
    }

    private void ClearResult()
    {
        _source = null;
        _truncated = false;
        _removedColumns = 0;
        _notes = [];
        OnPropertyChanged(nameof(Source));
        OnPropertyChanged(nameof(TruncationBanner));
        OnPropertyChanged(nameof(RemovedColumnsNote));
        OnPropertyChanged(nameof(CanExport));
        ResultChanged?.Invoke(this, EventArgs.Empty);
    }

    private void Fail(string message)
    {
        ClearResult();
        _message = message;
        OnPropertyChanged(nameof(Message));
        Phase = ReportPhase.Failed;
        OnPropertyChanged(nameof(StatusText));
    }

    /// <summary>How many columns a removal message reports: the first whole number in its text, else one.</summary>
    private static int CountIn(ReportMessage message)
    {
        var text = message.Text;
        for (var i = 0; i < text.Length; i++)
        {
            if (char.IsAsciiDigit(text[i]))
            {
                var end = i;
                while (end < text.Length && char.IsAsciiDigit(text[end]))
                {
                    end++;
                }

                return end - i <= 6 && int.TryParse(text.AsSpan(i, end - i), NumberStyles.None, CultureInfo.InvariantCulture, out var n) ? n : 1;
            }
        }

        return 1;
    }

    private async Task UpdateSettingsAsync(Func<ReportUserSettings, ReportUserSettings> change, CancellationToken cancellationToken)
    {
        var current = await _store.LoadAsync(_user, cancellationToken).ConfigureAwait(true);
        _settings = ReportUserSettings.Normalize(change(current));
        await _store.SaveAsync(_user, _settings, cancellationToken).ConfigureAwait(true);
    }
}
