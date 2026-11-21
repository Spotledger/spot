using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run;

/// One process version's runs, as the history list groups them. The heading goes through the
/// run-label pair, so the group is called what the organisation calls a run.
public sealed record RunHistoryGroup(string ProcessName, int Version, string Heading, IReadOnlyList<RunSummary> Runs);

/// One row of a run's node timeline: which node, what it did, how long it took and whether it took
/// the error branch. A retry is a row of its own, so the retry count is visible as rows, not a count.
public sealed record RunTimelineRow(
    int Order,
    string NodeId,
    string KindName,
    RunNodeStatus Status,
    string StatusText,
    int Attempt,
    DateTimeOffset StartedAt,
    DateTimeOffset? EndedAt,
    string? Duration,
    bool ErrorBranch,
    string? ErrorCode,
    string? ErrorMessage,
    bool IsRetry,
    int Attempts)
{

    /// The row as a person reads it, and as the saved text form writes it.
    public string Text
    {
        get
        {
            System.Text.StringBuilder builder = new();
            builder.Append(Order.ToString(CultureInfo.InvariantCulture))
                .Append(". ")
                .Append(NodeId)
                .Append(" (").Append(KindName).Append("): ")
                .Append(StatusText)
                .Append(", ").Append(RunStrings.AttemptText(Attempt));
            if (Duration is { } duration)
            {
                builder.Append(", ").Append(duration);
            }

            if (IsRetry)
            {
                builder.Append(", ").Append(RunStrings.Retry);
            }

            if (ErrorBranch)
            {
                builder.Append(", ").Append(RunStrings.ErrorBranch);
            }

            if (ErrorCode is { } code)
            {
                builder.Append(", ").Append(RunStrings.ErrorLabel).Append(' ').Append(code);
            }

            if (ErrorMessage is { } message)
            {
                builder.Append(": ").Append(message);
            }

            return builder.ToString();
        }
    }
}

/// One node's input and output panes, already redacted: the tree a pane paints, and the text it
/// copies out. A payload the client cannot parse is withheld instead of echoed.
public sealed record RunExchange(
    string NodeId,
    int Attempt,
    string Title,
    JsonTreeNode? Input,
    JsonTreeNode? Output,
    string InputText,
    string OutputText)
{

    /// The input pane's lines, indented by depth, one row per value.
    public IReadOnlyList<string> InputLines => Lines(Input, RunStrings.NoInput);

    /// The output pane's lines, indented by depth, one row per value.
    public IReadOnlyList<string> OutputLines => Lines(Output, RunStrings.NoOutput);

    private static List<string> Lines(JsonTreeNode? node, string whenEmpty)
    {
        IReadOnlyList<JsonTreeRow> rows = JsonTree.Rows(node);
        if (rows.Count == 0)
        {
            return [whenEmpty];
        }

        List<string> lines = [];
        foreach (JsonTreeRow row in rows)
        {
            string indent = new(' ', row.Depth * 2);
            string name = row.Name.Length > 0 && !row.Name.StartsWith('[')
                ? row.Name + ": "
                : string.Empty;
            lines.Add(indent + name + row.Display);
        }

        return lines;
    }
}

/// The execution history view model: run groups, one run's node timeline, and one exchange's input
/// and output trees. Everything it shows was redacted before it was kept here, so a pane can never
/// paint a value the server sent by mistake.
public sealed class RunHistory
{

    private readonly RunStrings _strings;

    private readonly FlowKindCatalogue _catalogue;

    public RunHistory(RunStrings strings, RunRedactor redactor, FlowKindCatalogue catalogue)
    {
        ArgumentNullException.ThrowIfNull(strings);
        ArgumentNullException.ThrowIfNull(redactor);
        ArgumentNullException.ThrowIfNull(catalogue);
        _strings = strings;
        Redactor = redactor;
        _catalogue = catalogue;
    }

    /// The pass every payload goes through before it is kept here. A host that learns a credential
    /// value later replaces it, and the next selection is redacted with the new set.
    public RunRedactor Redactor { get; set; }

    /// The run list, grouped by process and version.
    public IReadOnlyList<RunHistoryGroup> Groups { get; private set; } = [];

    /// The selected run's node timeline, oldest first.
    public IReadOnlyList<RunTimelineRow> Timeline { get; private set; } = [];

    /// The selected run's exchanges, one per attempt, in timeline order.
    public IReadOnlyList<RunExchange> Exchanges { get; private set; } = [];

    /// The run the timeline belongs to.
    public RunSummary? Selected { get; private set; }

    /// The heading over the timeline, or the empty-state words when no run is open.
    public string Heading => Selected is { } run ? _strings.RunHeading(run) : _strings.NoTimeline;

    /// How many attempts the timeline holds, which is what a person calls the retry count of a node.
    public int AttemptCount => Timeline.Count;

    /// Replaces the run list, keeping the newest run of each process version first.
    public void SetRuns(IEnumerable<RunSummary>? runs)
    {
        List<RunHistoryGroup> groups = [];
        if (runs is not null)
        {
            foreach (IGrouping<(string ProcessName, int Version), RunSummary> group in runs
                .Where(run => run is not null)
                .GroupBy(run => (run.ProcessName, run.Version))
                .OrderBy(group => group.Key.ProcessName, StringComparer.Ordinal)
                .ThenByDescending(group => group.Key.Version))
            {
                List<RunSummary> ordered = [.. group.OrderByDescending(run => run.StartedAt)];
                groups.Add(new RunHistoryGroup(
                    group.Key.ProcessName,
                    group.Key.Version,
                    _strings.GroupHeading(group.Key.ProcessName, group.Key.Version),
                    ordered));
            }
        }

        Groups = groups;
    }

    /// Opens one run: its timeline rows and its redacted exchanges.
    public void Select(RunDetail? detail)
    {
        if (detail is null)
        {
            Selected = null;
            Timeline = [];
            Exchanges = [];
            return;
        }

        Selected = detail.Summary;
        List<RunNodeAttempt> attempts = [.. detail.Attempts
            .OrderBy(attempt => attempt.StartedAt)
            .ThenBy(attempt => attempt.Attempt)];

        Dictionary<string, int> attemptsByNode = new(StringComparer.Ordinal);
        foreach (RunNodeAttempt attempt in attempts)
        {
            attemptsByNode[attempt.NodeId] = attemptsByNode.TryGetValue(attempt.NodeId, out int seen)
                ? Math.Max(seen, attempt.Attempt)
                : attempt.Attempt;
        }

        List<RunTimelineRow> rows = [];
        List<RunExchange> exchanges = [];
        for (int i = 0; i < attempts.Count; i++)
        {
            RunNodeAttempt attempt = attempts[i];
            int total = attemptsByNode[attempt.NodeId];

            // Both the error message and the panes go through the redactor again: the server
            // redacts before it records, and this pass makes a server bug harmless.
            string? message = attempt.ErrorMessage is { } raw ? Redactor.Redact(raw) : null;
            rows.Add(new RunTimelineRow(
                i + 1,
                attempt.NodeId,
                _catalogue.DisplayName(attempt.Kind),
                attempt.Status,
                RunStrings.StatusText(attempt.Status),
                attempt.Attempt,
                attempt.StartedAt,
                attempt.EndedAt,
                attempt.Duration is { } duration ? RunStrings.DurationText(duration) : null,
                attempt.ErrorBranch,
                attempt.ErrorCode is { } errorCode ? Redactor.Redact(errorCode) : null,
                message,
                attempt.Attempt > 1,
                total));

            exchanges.Add(new RunExchange(
                attempt.NodeId,
                attempt.Attempt,
                attempt.NodeId + " · " + RunStrings.AttemptText(attempt.Attempt),
                Redactor.RedactParsed(attempt.InputJson),
                Redactor.RedactParsed(attempt.OutputJson),
                PanelText(attempt.InputJson, RunStrings.NoInput),
                PanelText(attempt.OutputJson, RunStrings.NoOutput)));
        }

        Timeline = rows;
        Exchanges = exchanges;
    }

    /// The exchange of the attempt a person selected, or null when the run has none.
    public RunExchange? Exchange(string nodeId, int attempt)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        foreach (RunExchange exchange in Exchanges)
        {
            if (string.Equals(exchange.NodeId, nodeId, StringComparison.Ordinal) && exchange.Attempt == attempt)
            {
                return exchange;
            }
        }

        return null;
    }

    /// The history as the saved text form: the group, the run, every timeline row and every
    /// exchange's redacted payload. This is what "save the history" writes and what a test asserts.
    public string ToText()
    {
        StringBuilder builder = new();
        foreach (RunHistoryGroup group in Groups)
        {
            builder.Append(group.Heading).Append('\n');
            foreach (RunSummary run in group.Runs)
            {
                builder.Append("  ").Append(_strings.RunHeading(run)).Append('\n');
            }
        }

        if (Selected is { } selected)
        {
            if (Groups.Count > 0)
            {
                builder.Append('\n');
            }

            builder.Append(_strings.RunHeading(selected)).Append('\n');
            builder.Append(RunStrings.TimelineHeading).Append('\n');
            foreach (RunTimelineRow row in Timeline)
            {
                builder.Append(row.Text).Append('\n');
            }

            foreach (RunExchange exchange in Exchanges)
            {
                builder.Append('\n').Append(exchange.Title).Append('\n');
                builder.Append(RunStrings.InputPane).Append('\n').Append(exchange.InputText).Append('\n');
                builder.Append(RunStrings.OutputPane).Append('\n').Append(exchange.OutputText).Append('\n');
            }
        }
        else if (Groups.Count == 0)
        {
            builder.Append(_strings.NoRuns).Append('\n');
        }

        return builder.ToString();
    }

    private string PanelText(string? json, string whenEmpty)
    {
        if (string.IsNullOrWhiteSpace(json))
        {
            return whenEmpty;
        }

        string redacted = Redactor.RedactJson(json);
        return string.IsNullOrEmpty(redacted) ? whenEmpty : redacted;
    }
}
