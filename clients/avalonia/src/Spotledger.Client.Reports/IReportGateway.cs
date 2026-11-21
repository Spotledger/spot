namespace Spotledger.Client.Reports;

/// <summary>
/// The reports' seam to the server. Every method throws <see cref="Spotledger.Client.Api.ApiRequestException"/>
/// for a contract error and decides no permission: what the caller may see is what the server returns. The
/// production implementation is <see cref="NativeReportGateway"/>.
/// </summary>
public interface IReportGateway
{
    /// <summary>The reports the caller may see (<c>report_list</c>).</summary>
    Task<IReadOnlyList<ReportSummary>> ListAsync(CancellationToken cancellationToken);

    /// <summary>One report as described for the caller (<c>report_describe</c>). An unknown report and one the
    /// caller's roles do not admit both fail with <c>report.not_found</c>.</summary>
    Task<ReportDescription> DescribeAsync(string report, CancellationToken cancellationToken);

    /// <summary>Runs a report (<c>report_run</c>). <paramref name="filtersJson"/> is the JSON object of parameter
    /// values keyed by parameter name; <paramref name="limit"/> never exceeds the report's <c>max_rows</c>.</summary>
    Task<ReportRunResult> RunAsync(string report, string filtersJson, int limit, CancellationToken cancellationToken);

    /// <summary>True when the contract has an export operation this report maps to. Contract 0.7.0 has none for
    /// reports, so the production gateway answers false and the viewer writes the loaded rows itself.</summary>
    bool CanExportOnServer(ReportDescription description);

    /// <summary>Writes the CSV of the report run with the same parameters to <paramref name="destination"/> through the
    /// server's export operation. Only called when <see cref="CanExportOnServer"/> is true.</summary>
    Task ExportCsvAsync(ReportDescription description, string filtersJson, Stream destination, CancellationToken cancellationToken);
}

/// <summary>What the shell offers the report screens: opening another report in a tab and asking for a line of text.</summary>
public interface IReportShell
{
    /// <summary>Opens (or focuses) the tab of a report. <paramref name="parameters"/> are parameter values as text
    /// to start the form with; the tab identity is the report plus a digest of them.</summary>
    void OpenReport(string name, string label, IReadOnlyDictionary<string, string>? parameters);

    /// <summary>Asks for a line of text (the name of a parameter set). Null cancels.</summary>
    Task<string?> AskTextAsync(string title, string prompt);
}

/// <summary>A shell with no host: nothing opens and every question is declined.</summary>
public sealed class NullReportShell : IReportShell
{
    /// <inheritdoc />
    public void OpenReport(string name, string label, IReadOnlyDictionary<string, string>? parameters)
    {
    }

    /// <inheritdoc />
    public Task<string?> AskTextAsync(string title, string prompt) => Task.FromResult<string?>(null);
}

/// <summary>Where an export goes (a save dialog in the host; a memory stream in tests).</summary>
public interface IReportExportSink
{
    /// <summary>Opens the destination for writing, or returns null when the person cancelled. The caller disposes the stream.</summary>
    Task<Stream?> OpenAsync(string suggestedFileName, CancellationToken cancellationToken);
}
