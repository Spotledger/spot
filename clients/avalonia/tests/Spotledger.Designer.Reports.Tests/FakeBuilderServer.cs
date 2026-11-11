using System.Text.RegularExpressions;
using Spotledger.Designer.Reports;

namespace Spotledger.Designer.Reports.Tests;

/// <summary>
/// An in-memory server that behaves like spot-nocode-builders closely enough to prove the
/// commands the UI issues: it applies or refuses each op, keeps an op log with undo and redo,
/// validates in plain language, previews under a row cap and masks the columns the viewer may not
/// read. It records the wire form of every op so tests can compare it with the golden sequence.
/// </summary>
internal sealed partial class FakeBuilderServer : IBuilderServer
{
    private readonly List<(ReportDraft Draft, long Version)> _reportHistory = new();
    private int _reportCursor;
    private readonly List<(DashboardDraft Draft, long Version)> _dashHistory = new();
    private int _dashCursor;

    public FakeBuilderServer()
    {
        var draft = new ReportDraft("rpt-1", "sales_report", "Sales report", "Sales Invoice", "editing",
            [], [], [], [], [], [], [], null, []);
        _reportHistory.Add((draft, 1));
        var dash = new DashboardDraft("dash-1", "sales_board", "Sales board", "editing", 12, 8, new RefreshPolicy.OnOpen(), [], [], []);
        _dashHistory.Add((dash, 1));
    }

    /// <summary>The wire form of every op the server accepted, in order.</summary>
    public List<string> ReportOps { get; } = new();

    public List<string> DashboardOps { get; } = new();

    /// <summary>Fields the viewer has no right to read: the server masks these columns.</summary>
    public HashSet<string> MaskedFields { get; } = new();

    /// <summary>When true the server (wrongly) still sends values in masked columns.</summary>
    public bool LeakMaskedValues { get; set; }

    public int PreviewRowsAvailable { get; set; } = 200;

    public List<string> PublishedApprovals { get; } = new();

    public int LastPreviewCap { get; private set; }

    /// <summary>When true every call that changes or reads a draft fails like a dropped connection.</summary>
    public bool Offline { get; set; }

    /// <summary>When true a tile preview carries rows (the "territory" column masked for this viewer).</summary>
    public bool TilePreviewsCarryRows { get; set; }

    private void CheckOnline()
    {
        if (Offline)
        {
            throw new HttpRequestException("connection dropped");
        }
    }

    public static FieldOffers SalesOffers { get; } = new(
        "Sales Invoice",
        "Sales Invoice",
        [
            new FieldOffer("posting_date", "Posting date", OfferKind.Day, true, false),
            new FieldOffer("grand_total", "Grand total", OfferKind.Exact, false, true),
            new FieldOffer("qty", "Quantity", OfferKind.Whole, false, true),
            new FieldOffer("status", "Status", OfferKind.Text, true, false),
            new FieldOffer("customer", "Customer", OfferKind.Link, true, false),
            new FieldOffer("is_return", "Is return", OfferKind.Bool, true, false),
        ],
        [
            new JoinOffer("customer", "Customer", "Customer", "customer",
            [
                new FieldOffer("customer.territory", "Territory", OfferKind.Text, true, false),
            ]),
        ]);

    public Task<IReadOnlyList<Spotledger.Client.Designer.DocTypeChoice>> SearchDocTypesAsync(string query, CancellationToken cancellationToken = default) =>
        Task.FromResult<IReadOnlyList<Spotledger.Client.Designer.DocTypeChoice>>([new("Sales Invoice", "Sales Invoice")]);

    public Task<FieldOffers> OffersAsync(string docType, CancellationToken cancellationToken = default) => Task.FromResult(SalesOffers);

    // ---- reports ----------------------------------------------------------------------------

    private ReportDraft Current => _reportHistory[_reportCursor].Draft;

    private ReportOutcome Out() => new(Current, _reportHistory[_reportCursor].Version, null);

    public Task<ReportOutcome> OpenReportAsync(string draftId, CancellationToken cancellationToken = default) => Task.FromResult(Out());

    public Task<ReportOutcome> ApplyReportAsync(string draftId, ReportOp op, long baseVersion, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        if (baseVersion != _reportHistory[_reportCursor].Version)
        {
            return Task.FromResult(new ReportOutcome(null, baseVersion, Issue("stale", "The report changed somewhere else.", "Reload it.")));
        }

        var d = Current;
        ReportDraft? next;
        BuilderIssue? refusal = null;
        switch (op)
        {
            case ReportOp.AddColumn add:
                if (!SalesOffers.Fields.Any(f => f.Path == add.Field) && !SalesOffers.Joins.SelectMany(j => j.Fields).Any(f => f.Path == add.Field))
                {
                    refusal = Issue("unknown_field", $"There is no field called {add.Field} you can report on.", "Pick a field from the list.");
                }
                else if (add.Field.Contains('.', StringComparison.Ordinal) && d.Joins.All(j => j.Alias != add.Field.Split('.')[0]))
                {
                    refusal = Issue("no_link", "That field is reached through a link the report does not follow yet.", "Add the link first.");
                }

                next = d with { Columns = [.. d.Columns, new DraftColumn(add.Field, add.Label, add.Aggregate)] };
                break;
            case ReportOp.RemoveColumn r:
                next = d with { Columns = [.. d.Columns.Where(c => c.Field != r.Field)] };
                break;
            case ReportOp.SetColumnAggregate s:
                next = d with { Columns = [.. d.Columns.Select(c => c.Field == s.Field ? c with { Aggregate = s.Aggregate } : c)] };
                break;
            case ReportOp.AddJoin j:
                next = d with { Joins = [.. d.Joins, new DraftJoin(j.Alias, j.DocType, j.On)] };
                break;
            case ReportOp.AddParameter p:
                next = d with { Parameters = [.. d.Parameters, p.Parameter] };
                break;
            case ReportOp.RemoveParameter rp:
                next = d with { Parameters = [.. d.Parameters.Where(x => x.Name != rp.ParameterName)] };
                break;
            case ReportOp.AddFilter f:
                if (f.Filter.Target is FilterTarget.Parameter t && d.Parameters.All(x => x.Name != t.Name))
                {
                    refusal = Issue("unknown_parameter", "The filter asks the viewer for a value the report does not define.", "Add the parameter first.");
                }

                next = d with { Filters = [.. d.Filters, f.Filter] };
                break;
            case ReportOp.RemoveFilter rf:
                next = d with { Filters = [.. d.Filters.Where((_, i) => i != rf.Index)] };
                break;
            case ReportOp.GroupBy g:
                next = d with { GroupBy = [.. d.GroupBy, g.Field] };
                break;
            case ReportOp.UngroupBy ug:
                next = d with { GroupBy = [.. d.GroupBy.Where(x => x != ug.Field)] };
                break;
            case ReportOp.SortBy sb:
                next = d with { Sort = [.. d.Sort.Where(x => x.Field != sb.Field), new DraftSort(sb.Field, sb.Direction)] };
                break;
            case ReportOp.ClearSort:
                next = d with { Sort = [] };
                break;
            case ReportOp.SetTotals tot:
                next = d with { Totals = tot.Columns };
                break;
            case ReportOp.SetChart c:
                next = d with { Chart = c.Chart };
                break;
            case ReportOp.SetRoles roles:
                next = d with { Roles = roles.Roles };
                break;
            case ReportOp.SetLabel l:
                next = d with { Label = l.Label };
                break;
            case ReportOp.SetBase b:
                next = d with { Base = b.DocType, Columns = [], Joins = [], Filters = [], GroupBy = [], Sort = [], Totals = [], Chart = null };
                break;
            default:
                next = d;
                break;
        }

        if (refusal is not null)
        {
            return Task.FromResult(new ReportOutcome(null, baseVersion, refusal));
        }

        ReportOps.Add(op.ToJson().ToJsonString());
        _reportHistory.RemoveRange(_reportCursor + 1, _reportHistory.Count - _reportCursor - 1);
        _reportHistory.Add((next, baseVersion + 1));
        _reportCursor++;
        return Task.FromResult(Out());
    }

    public Task<ReportOutcome> UndoReportAsync(string draftId, CancellationToken cancellationToken = default)
    {
        if (_reportCursor > 0)
        {
            _reportCursor--;
        }

        return Task.FromResult(Out());
    }

    public Task<ReportOutcome> RedoReportAsync(string draftId, CancellationToken cancellationToken = default)
    {
        if (_reportCursor < _reportHistory.Count - 1)
        {
            _reportCursor++;
        }

        return Task.FromResult(Out());
    }

    public bool Saved { get; private set; }

    public Task SaveReportAsync(string draftId, long version, CancellationToken cancellationToken = default)
    {
        Saved = true;
        return Task.CompletedTask;
    }

    public Task<IReadOnlyList<BuilderIssue>> ValidateReportAsync(string draftId, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        var d = Current;
        var issues = new List<BuilderIssue>();
        if (d.Columns.Count == 0)
        {
            issues.Add(Issue("no_columns", "The report has no columns yet.", "Add at least one field."));
        }

        if (d.GroupBy.Count > 0)
        {
            foreach (var c in d.Columns.Where(c => c.Aggregate is null && !d.GroupBy.Contains(c.Field)))
            {
                issues.Add(Issue("ungrouped", $"{c.Field} is neither grouped nor summed.", "Group by it or choose a sum, count or average."));
            }
        }

        return Task.FromResult<IReadOnlyList<BuilderIssue>>(issues);
    }

    public Task<ReportRows> PreviewReportAsync(string draftId, int rowCap, CancellationToken cancellationToken = default)
    {
        LastPreviewCap = rowCap;
        return Task.FromResult(RowsFor(Current, rowCap));
    }

    public Task<PublishOutcome> PublishReportAsync(string draftId, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        return Task.FromResult(new PublishOutcome(true, "Published sales_report.", [], "sales_report"));
    }

    public Task<ReportRows> RunReportAsync(string reportName, IReadOnlyDictionary<string, FilterValue> parameters, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        LastRunParameters = new Dictionary<string, FilterValue>(parameters);
        return Task.FromResult(RowsFor(Current, 1000));
    }

    public Dictionary<string, FilterValue> LastRunParameters { get; private set; } = new();

    private ReportRows RowsFor(ReportDraft d, int cap)
    {
        var columns = d.Columns.Select(c => new PreviewColumn(c.Name, c.Label ?? c.Name, MaskedFields.Contains(c.Field))).ToList();
        var total = PreviewRowsAvailable;
        var shown = Math.Min(total, cap);
        var rows = new List<IReadOnlyList<string?>>();
        for (var i = 0; i < shown; i++)
        {
            rows.Add(columns.Select(c => c.Masked && !LeakMaskedValues ? null : $"{c.Name}-{i}").ToList());
        }

        return new ReportRows(columns, rows, total > shown, cap);
    }

    public Task<IReadOnlyList<ReportSummary>> ListReportsAsync(CancellationToken cancellationToken = default) =>
        Task.FromResult<IReadOnlyList<ReportSummary>>([new ReportSummary("sales_report", "Sales report", ["total", "territory"])]);

    // ---- dashboards -------------------------------------------------------------------------

    private DashboardDraft CurrentDash => _dashHistory[_dashCursor].Draft;

    private DashboardOutcome DashOut() => new(CurrentDash, _dashHistory[_dashCursor].Version, null);

    public Task<DashboardOutcome> OpenDashboardAsync(string draftId, CancellationToken cancellationToken = default) => Task.FromResult(DashOut());

    public Task<DashboardOutcome> ApplyDashboardAsync(string draftId, DashboardOp op, long baseVersion, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        var d = CurrentDash;
        DashboardDraft next;
        switch (op)
        {
            case DashboardOp.AddTile add:
                if (Misplaced(d, add.Tile.Place, add.Tile.Id) is { } bad)
                {
                    return Task.FromResult(new DashboardOutcome(null, baseVersion, bad));
                }

                next = d with { Tiles = [.. d.Tiles, add.Tile] };
                break;
            case DashboardOp.MoveTile move:
                if (Misplaced(d, move.Place, move.Id) is { } bad2)
                {
                    return Task.FromResult(new DashboardOutcome(null, baseVersion, bad2));
                }

                next = d with { Tiles = [.. d.Tiles.Select(t => t.Id == move.Id ? t with { Place = move.Place } : t)] };
                break;
            case DashboardOp.RemoveTile rm:
                next = d with { Tiles = [.. d.Tiles.Where(t => t.Id != rm.Id)] };
                break;
            case DashboardOp.SetTile st:
                next = d with { Tiles = [.. d.Tiles.Select(t => t.Id == st.Id ? t with { Title = st.Title, UsesFilters = st.UsesFilters } : t)] };
                break;
            case DashboardOp.AddFilter af:
                next = d with { Filters = [.. d.Filters, af.Filter] };
                break;
            case DashboardOp.RemoveFilter rf:
                next = d with { Filters = [.. d.Filters.Where(f => f.Name != rf.FilterName)] };
                break;
            case DashboardOp.SetRefresh sr:
                next = d with { Refresh = sr.Refresh };
                break;
            case DashboardOp.SetRoles roles:
                next = d with { Roles = roles.Roles };
                break;
            default:
                next = d;
                break;
        }

        DashboardOps.Add(op.ToJson().ToJsonString());
        _dashHistory.Add((next, baseVersion + 1));
        _dashCursor = _dashHistory.Count - 1;
        return Task.FromResult(DashOut());
    }

    private static BuilderIssue? Misplaced(DashboardDraft d, Place p, string id)
    {
        if (p.Row < 1 || p.Column < 1 || p.Width < 1 || p.Height < 1)
        {
            return Issue("bad_layout", "A tile needs a row, a column and at least one cell.", "Move it onto the grid.");
        }

        if (p.LastColumn > d.GridColumns || p.LastRow > d.GridRows)
        {
            return Issue("bad_layout", "A tile does not fit on the dashboard.", "Move it or make it smaller.");
        }

        return d.Tiles.Any(t => t.Id != id && t.Place.Overlaps(p))
            ? Issue("bad_layout", "Two tiles would sit on top of each other.", "Move it to a free place.")
            : null;
    }

    public Task<IReadOnlyList<BuilderIssue>> ValidateDashboardAsync(string draftId, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        var issues = CurrentDash.Tiles.Where(t => t.Report != "sales_report")
            .Select(t => Issue("unknown_report", $"{t.Title} draws on a report that is not published.", "Choose a published report."))
            .ToList();
        return Task.FromResult<IReadOnlyList<BuilderIssue>>(issues);
    }

    public Task<IReadOnlyList<TilePreview>> PreviewDashboardAsync(string draftId, IReadOnlyDictionary<string, FilterValue> filters, int rowCap, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        LastDashboardFilters = new Dictionary<string, FilterValue>(filters);
        ReportRows? rows = TilePreviewsCarryRows
            ? new ReportRows([new PreviewColumn("total", "Total", false), new PreviewColumn("territory", "Territory", true)], [["10.50", "north"], ["7.25", "south"]], false, rowCap)
            : null;
        return Task.FromResult<IReadOnlyList<TilePreview>>([.. CurrentDash.Tiles.Select(t => new TilePreview(t.Id, $"{t.Title} ({filters.Count} filters)", rows))]);
    }

    public Dictionary<string, FilterValue> LastDashboardFilters { get; private set; } = new();

    public Task<PublishOutcome> PublishDashboardAsync(string draftId, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        return Task.FromResult(new PublishOutcome(true, "Published sales_board.", [], "sales_board"));
    }

    // ---- approvals --------------------------------------------------------------------------

    [GeneratedRegex("^[a-z][a-z0-9_]*$")]
    private static partial Regex NameRule();

    public Task<IReadOnlyList<BuilderIssue>> ValidateApprovalAsync(ApprovalTemplate approval, CancellationToken cancellationToken = default)
    {
        CheckOnline();
        var issues = new List<BuilderIssue>();
        foreach (var who in new[] { approval.Approvers, approval.EscalateTo })
        {
            if (!NameRule().IsMatch(who.Name))
            {
                issues.Add(Issue("bad_approver", "An approver is not a name the site can resolve.", "Choose a role, a person, or a field that holds the approver."));
            }
        }

        return Task.FromResult<IReadOnlyList<BuilderIssue>>(issues);
    }

    public async Task<PublishOutcome> PublishApprovalAsync(ApprovalTemplate approval, CancellationToken cancellationToken = default)
    {
        var issues = await ValidateApprovalAsync(approval, cancellationToken);
        if (issues.Count > 0)
        {
            return new PublishOutcome(false, "The approval could not be published.", issues);
        }

        PublishedApprovals.Add(BuilderJson.ToJson(approval).ToJsonString());
        return new PublishOutcome(true, "Saved as a draft process.", [], approval.Name);
    }

    private static BuilderIssue Issue(string code, string message, string remedy) => new(code, true, null, message, remedy);
}
