namespace Spotledger.Designer.Reports;

/// <summary>
/// What the report builder, the dashboard builder and the approval wizard need from the server,
/// and nothing more (spot-nocode-builders).
/// </summary>
/// <remarks>
/// Every member is a call the server decides in: it offers only the fields the author may report
/// on, applies or refuses each op, validates, previews under a row cap, runs a published report
/// under the <em>viewer's</em> rights (so a column the viewer may not read comes back masked), and
/// checks <c>designer.publish</c>. The client never builds a query and never widens access. The
/// HTTP adapter belongs with the generated native client; the tests drive their own server.
/// </remarks>
public interface IBuilderServer
{
    /// <summary>Searches record types the author may report on.</summary>
    Task<IReadOnlyList<Spotledger.Client.Designer.DocTypeChoice>> SearchDocTypesAsync(string query, CancellationToken cancellationToken = default);

    /// <summary>The fields and links the author may put in a report over a base record type.</summary>
    Task<FieldOffers> OffersAsync(string docType, CancellationToken cancellationToken = default);

    /// <summary>Opens a report draft: the draft and its version.</summary>
    Task<ReportOutcome> OpenReportAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Applies one op at a version; a refusal is a plain-language issue.</summary>
    Task<ReportOutcome> ApplyReportAsync(string draftId, ReportOp op, long baseVersion, CancellationToken cancellationToken = default);

    /// <summary>Moves the op-log cursor back one op.</summary>
    Task<ReportOutcome> UndoReportAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Moves the op-log cursor forward one op.</summary>
    Task<ReportOutcome> RedoReportAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Saves the draft at a version.</summary>
    Task SaveReportAsync(string draftId, long version, CancellationToken cancellationToken = default);

    /// <summary>The server's plain-language validation of the draft.</summary>
    Task<IReadOnlyList<BuilderIssue>> ValidateReportAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>A bounded preview: at most <paramref name="rowCap"/> rows, masked columns marked.</summary>
    Task<ReportRows> PreviewReportAsync(string draftId, int rowCap, CancellationToken cancellationToken = default);

    /// <summary>Publishes the draft; the server checks the permission.</summary>
    Task<PublishOutcome> PublishReportAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Runs a published report for the current viewer with the given parameters.</summary>
    Task<ReportRows> RunReportAsync(string reportName, IReadOnlyDictionary<string, FilterValue> parameters, CancellationToken cancellationToken = default);

    /// <summary>Published reports a dashboard tile can draw on.</summary>
    Task<IReadOnlyList<ReportSummary>> ListReportsAsync(CancellationToken cancellationToken = default);

    /// <summary>Opens a dashboard draft.</summary>
    Task<DashboardOutcome> OpenDashboardAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Applies one dashboard op at a version.</summary>
    Task<DashboardOutcome> ApplyDashboardAsync(string draftId, DashboardOp op, long baseVersion, CancellationToken cancellationToken = default);

    /// <summary>The server's validation of the dashboard.</summary>
    Task<IReadOnlyList<BuilderIssue>> ValidateDashboardAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>A bounded preview of every tile, with the shared filter values applied.</summary>
    Task<IReadOnlyList<TilePreview>> PreviewDashboardAsync(string draftId, IReadOnlyDictionary<string, FilterValue> filters, int rowCap, CancellationToken cancellationToken = default);

    /// <summary>Publishes the dashboard.</summary>
    Task<PublishOutcome> PublishDashboardAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>The server's validation of an approval template (the same one publish runs).</summary>
    Task<IReadOnlyList<BuilderIssue>> ValidateApprovalAsync(ApprovalTemplate approval, CancellationToken cancellationToken = default);

    /// <summary>Publishes the approval as a draft process version; the server checks the permission.</summary>
    Task<PublishOutcome> PublishApprovalAsync(ApprovalTemplate approval, CancellationToken cancellationToken = default);
}

/// <summary>What happened to one dashboard op.</summary>
public sealed record DashboardOutcome(DashboardDraft? Draft, long Version, BuilderIssue? Refusal)
{
    /// <summary>Whether the server applied the op.</summary>
    public bool Ok => Refusal is null && Draft is not null;
}

/// <summary>A bounded preview of one tile.</summary>
public sealed record TilePreview(string TileId, string Summary, ReportRows? Rows)
{
    /// <summary>The tile's rows as the table shows them; masked columns carry only the mask.</summary>
    public RowsView TableView => Rows is null ? RowsView.Empty : RowsView.From(Rows, Rows.Cap > 0 ? Rows.Cap : ReportBuilderViewModel.DefaultPreviewRows);
}
