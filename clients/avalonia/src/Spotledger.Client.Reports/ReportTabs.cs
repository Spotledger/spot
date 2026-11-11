using Spotledger.Client.Shell;

namespace Spotledger.Client.Reports;

/// <summary>
/// The reports as workspace tab kinds and launcher commands: the catalogue (one tab) and a report (one tab per
/// report and parameter digest, so asking for the same report with the same parameters focuses the open tab).
/// </summary>
public static class ReportTabs
{
    /// <summary>The tab kind of one report.</summary>
    public const string ReportKind = "report";

    /// <summary>The tab kind of the catalogue.</summary>
    public const string CatalogueKind = "reports";

    /// <summary>The key of the catalogue tab.</summary>
    public const string CatalogueKey = "reports:catalogue";

    private const string ParameterPrefix = "p.";

    /// <summary>The unique key of a report tab: the report name plus the digest of its starting parameters.</summary>
    public static string KeyFor(string report, string digest) => $"report:{report}:{digest}";

    /// <summary>The request that opens the catalogue.</summary>
    public static WorkspaceTabRequest CatalogueRequest()
        => new(CatalogueKind, CatalogueKey, "Reports", new Dictionary<string, string>(StringComparer.Ordinal));

    /// <summary>The request that opens a report, optionally with starting parameter values as text.</summary>
    public static WorkspaceTabRequest ReportRequest(string name, string? label = null, IReadOnlyDictionary<string, string>? parameters = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(name);
        var arguments = new Dictionary<string, string>(StringComparer.Ordinal) { ["report"] = name };
        foreach (var (key, value) in parameters ?? new Dictionary<string, string>())
        {
            if (value.Length > 0)
            {
                arguments[ParameterPrefix + key] = value;
            }
        }

        return new WorkspaceTabRequest(ReportKind, KeyFor(name, ReportParameters.DigestOf(parameters)), label ?? name, arguments);
    }

    /// <summary>The report and starting parameter values a report request carries.</summary>
    public static (string Report, IReadOnlyDictionary<string, string> Parameters) ParseReportRequest(WorkspaceTabRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        var report = request.Arguments.TryGetValue("report", out var name) ? name : throw new ArgumentException("A report request names its report.", nameof(request));
        var parameters = request.Arguments
            .Where(p => p.Key.StartsWith(ParameterPrefix, StringComparison.Ordinal))
            .ToDictionary(p => p.Key[ParameterPrefix.Length..], p => p.Value, StringComparer.Ordinal);
        return (report, parameters);
    }

    /// <summary>Registers both kinds: <paramref name="catalogue"/> builds the catalogue model; <paramref name="report"/> builds
    /// the model of a report from its name and starting parameters (the host then calls <c>LoadAsync</c>).</summary>
    public static void Register(
        WorkspaceTabKinds kinds,
        Func<ReportCatalogueViewModel> catalogue,
        Func<string, IReadOnlyDictionary<string, string>, ReportViewModel> report)
    {
        ArgumentNullException.ThrowIfNull(kinds);
        ArgumentNullException.ThrowIfNull(catalogue);
        ArgumentNullException.ThrowIfNull(report);
        kinds.Register(CatalogueKind, _ => catalogue());
        kinds.Register(ReportKind, request =>
        {
            var (name, parameters) = ParseReportRequest(request);
            return report(name, parameters);
        });
    }

    /// <summary>Keeps a workspace tab's title in step with its report. Returns the subscription's disposer.</summary>
    public static IDisposable Bind(WorkspaceTabViewModel tab, ReportViewModel model)
    {
        ArgumentNullException.ThrowIfNull(tab);
        ArgumentNullException.ThrowIfNull(model);
        void Sync() => tab.Title = model.Title;
        void Changed(object? sender, System.ComponentModel.PropertyChangedEventArgs e)
        {
            if (e.PropertyName == nameof(ReportViewModel.Title))
            {
                Sync();
            }
        }

        model.PropertyChanged += Changed;
        Sync();
        return new Unsubscribe(() => model.PropertyChanged -= Changed);
    }

    private sealed class Unsubscribe(Action action) : IDisposable
    {
        public void Dispose() => action();
    }
}

/// <summary>The launcher commands of the reports: <c>Open report &lt;label&gt;</c>, registered per report at run time
/// (the shell learns the reports from the server), with ids <c>report.open.&lt;name&gt;</c>.</summary>
public static class ReportCommands
{
    /// <summary>The id prefix of the open commands.</summary>
    public const string OpenPrefix = "report.open.";

    /// <summary>The id of the open command of a report.</summary>
    public static string OpenId(string name) => OpenPrefix + name;

    /// <summary>The launcher definition of <c>Open report &lt;label&gt;</c>.</summary>
    public static CommandDefinition Open(ReportSummary report)
    {
        ArgumentNullException.ThrowIfNull(report);
        return new CommandDefinition(OpenId(report.Name), "Open report " + report.Label, $"Open the {report.Label} report.", "Reports", string.Empty, ' ')
        {
            SearchTerms = ["report", report.Label, report.Name, report.App],
        };
    }

    /// <summary>The report name a command id opens, or null when it is not a report command.</summary>
    public static string? TryParse(string commandId)
    {
        ArgumentNullException.ThrowIfNull(commandId);
        return commandId.StartsWith(OpenPrefix, StringComparison.Ordinal) && commandId.Length > OpenPrefix.Length ? commandId[OpenPrefix.Length..] : null;
    }

    /// <summary>The tab request a report command resolves to, or null.</summary>
    public static WorkspaceTabRequest? RequestFor(string commandId, string? label = null)
        => TryParse(commandId) is { } name ? ReportTabs.ReportRequest(name, label) : null;
}
