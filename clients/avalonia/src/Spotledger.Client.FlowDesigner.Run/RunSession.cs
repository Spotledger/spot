using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run;

/// One node's badge on the canvas: its state, its attempt, the word a person reads and the same
/// words in a sentence for the status list and a screen reader.
public sealed record RunNodeBadge(string NodeId, RunNodeStatus Status, int Attempt, string Text, string HelpText);

/// One test-run session: the sample data, the dry run the server executes, the step-through
/// controls and the history of the runs of this process version.
///
/// Nothing here decides what a run does: the server owns the graph, the sample, the order and the
/// timestamps. The session only carries the answers, and it never reads the wall clock — every time
/// it shows is the server's.
public sealed class FlowRunSession
{

    private readonly IRunServer _server;

    private readonly List<RunSecret> _secrets;

    public FlowRunSession(
        IRunServer server,
        FlowDocument document,
        FlowKindCatalogue catalogue,
        RunStrings? strings = null,
        IEnumerable<RunSecret>? secrets = null)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(catalogue);
        _server = server;
        Document = document;
        Catalogue = catalogue;
        Strings = strings ?? new RunStrings();
        _secrets = secrets is null ? [] : [.. secrets];
        Redactor = new RunRedactor(_secrets);
        SampleData = new SampleDataEditor(document, catalogue);
        History = new RunHistory(Strings, Redactor, catalogue);
    }

    public FlowDocument Document { get; }

    public FlowKindCatalogue Catalogue { get; }

    public RunStrings Strings { get; }

    public SampleDataEditor SampleData { get; }

    public RunHistory History { get; }

    /// The pass every payload and every message goes through before it is shown.
    public RunRedactor Redactor { get; private set; }

    /// The dry run in progress or just finished, as the server last described it.
    public RunSnapshot? Snapshot { get; private set; }

    /// The run the history has open.
    public RunDetail? Detail { get; private set; }

    /// The runs of this process version, newest first.
    public IReadOnlyList<RunSummary> Runs { get; private set; } = [];

    public string? StatusMessage { get; private set; }

    public string? ErrorMessage { get; private set; }

    public bool IsBusy { get; private set; }

    public bool HasRun => Snapshot is not null;

    public bool IsRunning => Snapshot is { Outcome: RunOutcome.Running };

    public bool CanStep => Snapshot is { Outcome: RunOutcome.Running } snapshot && snapshot.CanStep;

    public bool CanContinue => Snapshot is { Outcome: RunOutcome.Running } snapshot && snapshot.CanContinue;

    public bool CanStop => IsRunning;

    /// The credential values the client can scrub. A host adds one when it opens a credential for
    /// the run; the redactor then folds it into every pane and every message.
    public IReadOnlyList<RunSecret> Secrets => _secrets;

    public event EventHandler? Changed;

    /// Every node's badge, in definition order, or an empty list before the first run.
    public IReadOnlyList<RunNodeBadge> Badges
    {
        get
        {
            if (Snapshot is null)
            {
                return [];
            }

            List<RunNodeBadge> badges = [];
            foreach (FlowNode node in Document.Nodes)
            {
                RunNodeBadge? badge = Badge(node.Id);
                if (badge is not null)
                {
                    badges.Add(badge);
                }
            }

            return badges;
        }
    }

    /// One node's badge, or null when no run has started.
    public RunNodeBadge? Badge(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        if (Snapshot is not { } snapshot)
        {
            return null;
        }

        RunNodeSnapshot? node = snapshot.Node(nodeId);
        RunNodeStatus status = node?.Status ?? RunNodeStatus.Pending;
        int attempt = node?.Attempt ?? 1;
        return new RunNodeBadge(
            nodeId,
            status,
            attempt,
            RunStrings.BadgeText(status),
            RunStrings.BadgeHelp(nodeId, status, attempt));
    }

    /// Adds a credential value the client will scrub from everything it shows.
    public void AddSecret(RunSecret secret)
    {
        ArgumentNullException.ThrowIfNull(secret);
        _secrets.Add(secret);
        Redactor = new RunRedactor(_secrets);
        History.Redactor = Redactor;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    /// Starts a dry run with the sample data. The process is sent as its canonical text form, the
    /// same bytes a save sends, so the server executes exactly the version a person is looking at.
    /// Sample data that does not parse never reaches the server.
    public async Task<FlowActionResult> StartTestAsync(CancellationToken cancellationToken)
    {
        SampleDataCheck check = SampleData.Check();
        if (!check.Ok)
        {
            string reason = check.Problems.Count > 0 ? check.Problems[0].Message : RunStrings.SampleDataInvalid;
            return Refuse(reason);
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            DryRunRequest request = new(Document.Name, Document.Version, Document.ToText(), SampleData.Text);
            RunSnapshot snapshot = await _server.StartDryRunAsync(request, cancellationToken).ConfigureAwait(true);
            Apply(snapshot, out _);
            StatusMessage = Strings.TestStarted(snapshot.RunId);
            ErrorMessage = null;
            return FlowActionResult.Success(StatusMessage);
        }
        catch (RunRefusedException refused)
        {
            return Refuse(refused.Message);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    /// Advances the run by exactly one node.
    public Task<FlowActionResult> StepAsync(CancellationToken cancellationToken) =>
        AdvanceAsync(RunStepMode.Step, cancellationToken);

    /// Advances the run as far as the server takes it in one answer.
    public Task<FlowActionResult> ContinueAsync(CancellationToken cancellationToken) =>
        AdvanceAsync(RunStepMode.Continue, cancellationToken);

    /// Stops the run: the server decides what a stopped run's nodes say, and the history is loaded
    /// as it stands.
    public async Task<FlowActionResult> StopAsync(CancellationToken cancellationToken)
    {
        if (Snapshot is not { } snapshot)
        {
            return Refuse(RunStrings.NoRunYet);
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            RunSnapshot stopped = await _server.StopAsync(snapshot.RunId, cancellationToken).ConfigureAwait(true);
            Apply(stopped, out _);
            await LoadDetailAsync(stopped.RunId, cancellationToken).ConfigureAwait(true);
            StatusMessage = Strings.TestStopped;
            ErrorMessage = null;
            return FlowActionResult.Success(StatusMessage);
        }
        catch (RunRefusedException refused)
        {
            return Refuse(refused.Message);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    /// Loads the runs of this process version and groups them for the history list.
    public async Task<IReadOnlyList<RunSummary>> LoadRunsAsync(CancellationToken cancellationToken)
    {
        Runs = await _server
            .ListRunsAsync(new RunQuery(Document.Name, Document.Version), cancellationToken)
            .ConfigureAwait(true);
        History.SetRuns(Runs);
        Changed?.Invoke(this, EventArgs.Empty);
        return Runs;
    }

    /// Opens one run's node timeline and exchanges in the history view.
    public async Task<RunDetail?> OpenRunAsync(string runId, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(runId);
        try
        {
            Detail = await _server.LoadRunAsync(runId, cancellationToken).ConfigureAwait(true);
        }
        catch (RunRefusedException refused)
        {
            Refuse(refused.Message);
            return null;
        }

        History.Select(Detail);
        Changed?.Invoke(this, EventArgs.Empty);
        return Detail;
    }

    /// Reports a problem the host found, already scrubbed.
    public void ReportError(string message)
    {
        ErrorMessage = Redactor.Redact(message);
        StatusMessage = null;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    private async Task<FlowActionResult> AdvanceAsync(RunStepMode mode, CancellationToken cancellationToken)
    {
        if (Snapshot is not { } snapshot)
        {
            return Refuse(RunStrings.NoRunYet);
        }

        if (snapshot.Outcome != RunOutcome.Running)
        {
            return Refuse(Strings.TestFinished(snapshot.Outcome));
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            RunSnapshot advanced = await _server.StepAsync(snapshot.RunId, mode, cancellationToken).ConfigureAwait(true);
            Apply(advanced, out string? changedNode);
            if (advanced.Outcome == RunOutcome.Running)
            {
                StatusMessage = RunStrings.TestStepped(changedNode);
                ErrorMessage = null;
                return FlowActionResult.Success(StatusMessage);
            }

            // The run has ended, so its history exists: open it, which is what the brief asks a
            // person to see after a test run.
            await LoadDetailAsync(advanced.RunId, cancellationToken).ConfigureAwait(true);
            StatusMessage = Strings.TestFinished(advanced.Outcome);
            ErrorMessage = null;
            return FlowActionResult.Success(StatusMessage);
        }
        catch (RunRefusedException refused)
        {
            return Refuse(refused.Message);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    private async Task LoadDetailAsync(string runId, CancellationToken cancellationToken)
    {
        Detail = await _server.LoadRunAsync(runId, cancellationToken).ConfigureAwait(true);
        History.Select(Detail);
    }

    /// Keeps the new snapshot and reports the first node whose state changed, so the status line
    /// names what the step did rather than repeating the whole graph.
    private void Apply(RunSnapshot snapshot, out string? changedNode)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        changedNode = null;
        RunSnapshot? previous = Snapshot;
        Snapshot = snapshot;
        foreach (RunNodeSnapshot node in snapshot.Nodes)
        {
            if (previous is null)
            {
                if (node.Status != RunNodeStatus.Pending)
                {
                    changedNode = node.NodeId;
                    return;
                }

                continue;
            }

            if (previous.StatusOf(node.NodeId) != node.Status)
            {
                changedNode = node.NodeId;
                return;
            }
        }
    }

    private FlowActionResult Refuse(string? reason)
    {
        ErrorMessage = Redactor.Redact(reason ?? RunStrings.StepRefused(string.Empty));
        StatusMessage = null;
        Changed?.Invoke(this, EventArgs.Empty);
        return FlowActionResult.Refused(ErrorMessage);
    }
}
