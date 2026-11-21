using System.Collections.ObjectModel;

namespace Spotledger.Designer.Reports;

/// <summary>
/// The report builder: pick a base record type, add columns (following links, shown as
/// "Customer &gt; Territory"), filters with typed editors and a parameter toggle, grouping and
/// aggregates, sort, totals and a chart; a live bounded preview; save, publish, run and share.
/// </summary>
/// <remarks>
/// Every change is one op sent to the server, which applies or refuses it; this view-model only
/// ever shows the draft the server returned. It composes no query and never decides who may see
/// a column. Everything is a command, so the builder is fully usable by keyboard.
/// </remarks>
public sealed class ReportBuilderViewModel : ObservableObject
{
    /// <summary>The preview row cap used when none is given.</summary>
    public const int DefaultPreviewRows = 50;

    private readonly IBuilderServer _server;
    private readonly string _draftId;
    private readonly int _previewRows;
    private ReportDraft? _draft;
    private long _version;
    private FieldOffers? _offers;
    private RowsView _preview = RowsView.Empty;
    private RowsView _runResult = RowsView.Empty;
    private string _status = string.Empty;
    private string _publishMessage = string.Empty;

    /// <summary>Creates the builder over a server draft.</summary>
    public ReportBuilderViewModel(IBuilderServer server, string draftId, int previewRows = DefaultPreviewRows)
    {
        _server = server ?? throw new ArgumentNullException(nameof(server));
        _draftId = draftId ?? throw new ArgumentNullException(nameof(draftId));
        _previewRows = previewRows > 0 ? previewRows : DefaultPreviewRows;
        UndoCommand = new AsyncRelayCommand(async _ => await ApplyOutcomeAsync(await _server.UndoReportAsync(_draftId).ConfigureAwait(true)).ConfigureAwait(true));
        RedoCommand = new AsyncRelayCommand(async _ => await ApplyOutcomeAsync(await _server.RedoReportAsync(_draftId).ConfigureAwait(true)).ConfigureAwait(true));
        SaveCommand = new AsyncRelayCommand(_ => SaveAsync());
        PublishCommand = new AsyncRelayCommand(_ => PublishAsync(), _ => CanPublish);
        RefreshCommand = new AsyncRelayCommand(_ => RefreshAsync());
        RunCommand = new AsyncRelayCommand(_ => RunWithInputsAsync(), _ => Draft is not null);
        RemoveColumnCommand = new AsyncRelayCommand(field => field is string f ? RemoveColumnAsync(f) : Task.CompletedTask);
        RemoveFilterCommand = new AsyncRelayCommand(index => index is int i ? RemoveFilterAsync(i) : Task.CompletedTask);

        // A failure that escapes a command is shown in the status line, never swallowed.
        foreach (var command in new[] { UndoCommand, RedoCommand, SaveCommand, PublishCommand, RefreshCommand, RunCommand, RemoveColumnCommand, RemoveFilterCommand })
        {
            command.ExecutionFailed += exception => Status = Guard.Describe(exception);
        }
    }

    /// <summary>Runs the published report with the values typed into <see cref="ParameterInputs"/>.</summary>
    public AsyncRelayCommand RunCommand { get; }

    /// <summary>Removes the column named by the command parameter (a field path).</summary>
    public AsyncRelayCommand RemoveColumnCommand { get; }

    /// <summary>Removes the filter at the position given as the command parameter.</summary>
    public AsyncRelayCommand RemoveFilterCommand { get; }

    /// <summary>Base record types matching the last search, for the base picker.</summary>
    public ObservableCollection<Spotledger.Client.Designer.DocTypeChoice> BaseChoices { get; } = new();

    /// <summary>One input per report parameter, for running the report.</summary>
    public ObservableCollection<ParameterInput> ParameterInputs { get; } = new();

    /// <summary>Searches the record types the author may report on (the base picker).</summary>
    public async Task SearchBaseAsync(string query, CancellationToken cancellationToken = default)
    {
        var found = new List<Spotledger.Client.Designer.DocTypeChoice>();
        var ok = await Guard.RunAsync(async () =>
        {
            found.AddRange(await _server.SearchDocTypesAsync(query ?? string.Empty, cancellationToken).ConfigureAwait(true));
            return true;
        }, message => Status = message).ConfigureAwait(true);
        if (!ok)
        {
            return;
        }

        BaseChoices.Clear();
        foreach (var choice in found)
        {
            BaseChoices.Add(choice);
        }
    }

    /// <summary>The filters in words ("Status = Open", "Date at least the viewer's choice"), in order.</summary>
    public IReadOnlyList<string> FilterLines => Draft?.Filters.Select(f => $"{f.Field} {OperatorText(f.Op)} {TargetText(f.Target)}").ToList() ?? new List<string>();

    /// <summary>What the report is grouped by, in words.</summary>
    public string GroupText => Draft is { GroupBy.Count: > 0 } d ? "Grouped by " + string.Join(", ", d.GroupBy) : "Not grouped";

    /// <summary>The sort keys in words.</summary>
    public string SortText => Draft is { Sort.Count: > 0 } d ? "Sorted by " + string.Join(", ", d.Sort.Select(k => $"{k.Field} ({(k.Direction == SortDirection.Asc ? "A to Z" : "Z to A")})")) : "Not sorted";

    /// <summary>The footer totals in words.</summary>
    public string TotalsText => Draft is { Totals.Count: > 0 } d ? "Totals of " + string.Join(", ", d.Totals) : "No totals";

    /// <summary>The chart in words.</summary>
    public string ChartText => Draft?.Chart is { } c ? $"{c.Kind} chart of {string.Join(", ", c.Series)} by {c.X ?? "row"}" : "No chart";

    private static string OperatorText(Compare op) => op switch
    {
        Compare.Eq => "is",
        Compare.Ne => "is not",
        Compare.Lt => "is below",
        Compare.Le => "is at most",
        Compare.Gt => "is above",
        Compare.Ge => "is at least",
        _ => op.ToString(),
    };

    private static string TargetText(FilterTarget target) => target switch
    {
        FilterTarget.Parameter p => $"(the viewer chooses: {p.Name})",
        FilterTarget.Literal { Value: FilterValue.Text t } => t.Value,
        FilterTarget.Literal { Value: FilterValue.Whole w } => w.Value.ToString(System.Globalization.CultureInfo.InvariantCulture),
        FilterTarget.Literal { Value: FilterValue.Exact e } => e.Value,
        FilterTarget.Literal { Value: FilterValue.Day d } => d.Value,
        FilterTarget.Literal { Value: FilterValue.Bool b } => b.Value ? "yes" : "no",
        _ => string.Empty,
    };

    /// <summary>The columns' output names, for the chart pickers.</summary>
    public IReadOnlyList<string> ColumnFields => Draft?.Columns.Select(c => c.Field).ToList() ?? new List<string>();

    /// <summary>Undo the last op (the server's op log).</summary>
    public AsyncRelayCommand UndoCommand { get; }

    /// <summary>Redo the op just undone.</summary>
    public AsyncRelayCommand RedoCommand { get; }

    /// <summary>Save the draft.</summary>
    public AsyncRelayCommand SaveCommand { get; }

    /// <summary>Publish the report (enabled when the server's validation has no blocking issue).</summary>
    public AsyncRelayCommand PublishCommand { get; }

    /// <summary>Re-validate and refresh the preview.</summary>
    public AsyncRelayCommand RefreshCommand { get; }

    /// <summary>The draft as the server last returned it.</summary>
    public ReportDraft? Draft
    {
        get => _draft;
        private set
        {
            if (SetField(ref _draft, value))
            {
                OnPropertyChanged(nameof(ColumnFields));
                OnPropertyChanged(nameof(FilterLines));
                OnPropertyChanged(nameof(GroupText));
                OnPropertyChanged(nameof(SortText));
                OnPropertyChanged(nameof(TotalsText));
                OnPropertyChanged(nameof(ChartText));
                SyncParameterInputs();
                RunCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>The draft's version, which the next op is applied at.</summary>
    public long Version => _version;

    /// <summary>The fields and links the author may use.</summary>
    public FieldOffers? Offers
    {
        get => _offers;
        private set => SetField(ref _offers, value);
    }

    /// <summary>The palette rows: base fields, then each link's fields as "Link &gt; Field".</summary>
    public ObservableCollection<FieldChoice> FieldChoices { get; } = new();

    /// <summary>The server's validation issues in plain language.</summary>
    public ObservableCollection<BuilderIssue> Issues { get; } = new();

    /// <summary>The live bounded preview.</summary>
    public RowsView Preview
    {
        get => _preview;
        private set => SetField(ref _preview, value);
    }

    /// <summary>The result of the last run of the published report.</summary>
    public RowsView RunResult
    {
        get => _runResult;
        private set => SetField(ref _runResult, value);
    }

    /// <summary>The last thing that happened, or the last refusal, in plain words.</summary>
    public string Status
    {
        get => _status;
        private set => SetField(ref _status, value);
    }

    /// <summary>What the last publish said.</summary>
    public string PublishMessage
    {
        get => _publishMessage;
        private set => SetField(ref _publishMessage, value);
    }

    /// <summary>Whether validation reports no blocking issue and there is something to publish.</summary>
    public bool CanPublish => Draft is { Columns.Count: > 0 } && Issues.All(i => !i.IsError);

    /// <summary>Opens the draft, the field palette, the validation and the preview.</summary>
    public async Task OpenAsync(CancellationToken cancellationToken = default)
    {
        ReportOutcome? outcome = null;
        if (!await Guard.RunAsync(async () =>
        {
            outcome = await _server.OpenReportAsync(_draftId, cancellationToken).ConfigureAwait(true);
            return true;
        }, message => Status = message).ConfigureAwait(true) || outcome is null)
        {
            return;
        }

        if (!outcome.Ok)
        {
            Status = outcome.Refusal?.Message ?? "The report could not be opened.";
            return;
        }

        Take(outcome);
        await LoadOffersAsync(cancellationToken).ConfigureAwait(true);
        await RefreshAsync(cancellationToken).ConfigureAwait(true);
    }

    /// <summary>Chooses the base record type; clears what named the old one.</summary>
    public async Task<bool> SetBaseAsync(string docType)
    {
        if (!await ApplyAsync(new ReportOp.SetBase(docType)).ConfigureAwait(true))
        {
            return false;
        }

        await LoadOffersAsync().ConfigureAwait(true);
        return true;
    }

    /// <summary>Sets the report's heading.</summary>
    public Task<bool> SetLabelAsync(string label) => ApplyAsync(new ReportOp.SetLabel(label));

    /// <summary>Adds a column; a link the field is reached through is declared first.</summary>
    public async Task<bool> AddColumnAsync(FieldChoice choice, string? label = null, Aggregate? aggregate = null)
    {
        ArgumentNullException.ThrowIfNull(choice);
        if (choice.JoinAlias is { } alias && Draft?.Joins.All(j => j.Alias != alias) == true)
        {
            var join = Offers?.Joins.FirstOrDefault(j => j.Alias == alias);
            if (join is null)
            {
                Status = "That field is not available to you.";
                return false;
            }

            if (!await ApplyAsync(new ReportOp.AddJoin(join.Alias, join.DocType, join.On)).ConfigureAwait(true))
            {
                return false;
            }
        }

        return await ApplyAsync(new ReportOp.AddColumn(choice.Path, label, aggregate)).ConfigureAwait(true);
    }

    /// <summary>Removes a column by field path.</summary>
    public Task<bool> RemoveColumnAsync(string field) => ApplyAsync(new ReportOp.RemoveColumn(field));

    /// <summary>Sets or clears a column's aggregate.</summary>
    public Task<bool> SetAggregateAsync(string field, Aggregate? aggregate) => ApplyAsync(new ReportOp.SetColumnAggregate(field, aggregate));

    /// <summary>Groups by a field, or stops grouping by it when the report already does.</summary>
    public Task<bool> ToggleGroupAsync(string field) =>
        Draft?.GroupBy.Contains(field) == true
            ? ApplyAsync(new ReportOp.UngroupBy(field))
            : ApplyAsync(new ReportOp.GroupBy(field));

    /// <summary>Sorts by a column or field.</summary>
    public Task<bool> SortByAsync(string field, SortDirection direction) => ApplyAsync(new ReportOp.SortBy(field, direction));

    /// <summary>Removes every sort key.</summary>
    public Task<bool> ClearSortAsync() => ApplyAsync(new ReportOp.ClearSort());

    /// <summary>Sets the footer totals.</summary>
    public Task<bool> SetTotalsAsync(IReadOnlyList<string> columns) => ApplyAsync(new ReportOp.SetTotals(columns));

    /// <summary>Sets or clears the chart.</summary>
    public Task<bool> SetChartAsync(ChartSpec? chart) => ApplyAsync(new ReportOp.SetChart(chart));

    /// <summary>Starts a filter editor for a field of the palette.</summary>
    public static FilterEditor NewFilter(FieldChoice field) => new(field);

    /// <summary>Adds the filter the editor describes (and its parameter), or reports what is wrong with the value.</summary>
    public async Task<bool> AddFilterAsync(FilterEditor editor)
    {
        ArgumentNullException.ThrowIfNull(editor);
        if (!editor.TryBuild(out var ops))
        {
            Status = editor.Error;
            return false;
        }

        foreach (var op in ops)
        {
            if (op is ReportOp.AddParameter add && Draft?.Parameters.Any(p => p.Name == add.Parameter.Name) == true)
            {
                continue;
            }

            if (!await ApplyAsync(op).ConfigureAwait(true))
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>Removes the filter at a position.</summary>
    public Task<bool> RemoveFilterAsync(int index) => ApplyAsync(new ReportOp.RemoveFilter(index));

    /// <summary>Shares the report with the roles that may open it.</summary>
    public Task<bool> ShareAsync(IReadOnlyList<string> roles) => ApplyAsync(new ReportOp.SetRoles(roles));

    /// <summary>Applies one op; on a refusal the draft stays as it was and the status says why.</summary>
    public async Task<bool> ApplyAsync(ReportOp op)
    {
        ArgumentNullException.ThrowIfNull(op);
        return await Guard.RunAsync(async () =>
        {
            var outcome = await _server.ApplyReportAsync(_draftId, op, _version).ConfigureAwait(true);
            if (!outcome.Ok)
            {
                Status = outcome.Refusal is { } refusal ? $"{refusal.Message} {refusal.Remedy}".Trim() : "That change was not accepted.";
                return false;
            }

            await ApplyOutcomeAsync(outcome).ConfigureAwait(true);
            return true;
        }, message => Status = message).ConfigureAwait(true);
    }

    /// <summary>Validates and refreshes the bounded preview.</summary>
    public async Task RefreshAsync(CancellationToken cancellationToken = default)
    {
        await Guard.RunAsync(async () =>
        {
            var issues = await _server.ValidateReportAsync(_draftId, cancellationToken).ConfigureAwait(true);
            Issues.Clear();
            foreach (var issue in issues)
            {
                Issues.Add(issue);
            }

            OnPropertyChanged(nameof(CanPublish));
            PublishCommand.RaiseCanExecuteChanged();
            if (Draft is { Columns.Count: > 0 })
            {
                var rows = await _server.PreviewReportAsync(_draftId, _previewRows, cancellationToken).ConfigureAwait(true);
                Preview = RowsView.From(rows, _previewRows);
            }
            else
            {
                Preview = RowsView.Empty;
            }

            return true;
        }, message => Status = message).ConfigureAwait(true);
    }

    /// <summary>Runs the published report as the current viewer.</summary>
    public async Task RunAsync(IReadOnlyDictionary<string, FilterValue>? parameters = null)
    {
        if (Draft is null)
        {
            return;
        }

        await Guard.RunAsync(async () =>
        {
            var rows = await _server.RunReportAsync(Draft.Name, parameters ?? new Dictionary<string, FilterValue>(), CancellationToken.None).ConfigureAwait(true);
            RunResult = RowsView.From(rows, rows.Cap > 0 ? rows.Cap : _previewRows);
            return true;
        }, message => Status = message).ConfigureAwait(true);
    }

    /// <summary>Reads the typed parameter values and runs the published report; a bad value is named, not sent.</summary>
    public async Task RunWithInputsAsync()
    {
        if (!ParameterInput.TryCollect(ParameterInputs, out var values, out var error))
        {
            Status = error;
            return;
        }

        await RunAsync(values).ConfigureAwait(true);
    }

    private void SyncParameterInputs()
    {
        var wanted = _draft?.Parameters ?? (IReadOnlyList<DraftParameter>)Array.Empty<DraftParameter>();
        if (wanted.Select(p => p.Name).SequenceEqual(ParameterInputs.Select(p => p.Name)))
        {
            return;
        }

        ParameterInputs.Clear();
        foreach (var parameter in wanted)
        {
            ParameterInputs.Add(new ParameterInput(parameter.Name, parameter.Label, parameter.Type, parameter.Default));
        }
    }

    /// <summary>Runs a view's action; a failed server call is shown in the status line instead of escaping.</summary>
    public Task<bool> TryAsync(Func<Task<bool>> work) => Guard.RunAsync(work, message => Status = message);

    /// <summary>Adds the column to the footer totals, or takes it out when it is already there.</summary>
    public Task<bool> ToggleTotalAsync(string field)
    {
        var totals = Draft?.Totals.ToList() ?? new List<string>();
        if (!totals.Remove(field))
        {
            totals.Add(field);
        }

        return SetTotalsAsync(totals);
    }

    /// <summary>Draws the report as a chart of this kind, with an x column and the series columns.</summary>
    public Task<bool> SetChartAsync(ChartKind kind, string? x, IReadOnlyList<string> series, string? title = null) =>
        SetChartAsync(new ChartSpec(kind, x, series, title));

    private async Task SaveAsync()
    {
        await _server.SaveReportAsync(_draftId, _version).ConfigureAwait(true);
        Status = "Saved.";
    }

    private async Task PublishAsync()
    {
        var result = await _server.PublishReportAsync(_draftId).ConfigureAwait(true);
        PublishMessage = result.Message;
        Status = result.Ok ? "Published." : result.Message;
        if (!result.Ok)
        {
            Issues.Clear();
            foreach (var issue in result.Issues)
            {
                Issues.Add(issue);
            }
        }
    }

    private async Task LoadOffersAsync(CancellationToken cancellationToken = default)
    {
        FieldChoices.Clear();
        if (Draft is null)
        {
            return;
        }

        Offers = await _server.OffersAsync(Draft.Base, cancellationToken).ConfigureAwait(true);
        foreach (var field in Offers.Fields)
        {
            FieldChoices.Add(new FieldChoice(field.Path, field.Label, field, null));
        }

        foreach (var join in Offers.Joins)
        {
            foreach (var field in join.Fields)
            {
                FieldChoices.Add(new FieldChoice(field.Path, $"{join.Label} > {field.Label}", field, join.Alias));
            }
        }
    }

    private async Task ApplyOutcomeAsync(ReportOutcome outcome)
    {
        if (!outcome.Ok)
        {
            Status = outcome.Refusal?.Message ?? "Nothing to change.";
            return;
        }

        Take(outcome);
        await RefreshAsync().ConfigureAwait(true);
    }

    private void Take(ReportOutcome outcome)
    {
        Draft = outcome.Draft;
        _version = outcome.Version;
        OnPropertyChanged(nameof(Version));
        Status = string.Empty;
    }
}
