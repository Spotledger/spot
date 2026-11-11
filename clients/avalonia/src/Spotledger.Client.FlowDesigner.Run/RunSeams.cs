using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner.Run;

/// How the client asks the server to advance a dry run: one node, or as far as it goes.
public enum RunStepMode
{
    Step,
    Continue,
}

/// The state of one node in a run. The tokens are the contract's; a person reads the words in
/// <see cref="RunStrings"/>.
public enum RunNodeStatus
{
    Pending,
    Running,
    Succeeded,
    Failed,
    Waiting,
    Skipped,
    Stopped,
}

/// How a whole run ended, so far.
public enum RunOutcome
{
    Running,
    Succeeded,
    Failed,
    Stopped,
}

/// The state tokens, exactly as the server's <c>NodeOutcome</c> and history record spell them
/// (spot-bpm-integration::history::RecordState and the engine's outcome): code keeps the
/// internal vocabulary, a person never reads it.
public static class RunTokens
{

    public static string Token(this RunNodeStatus status) => status switch
    {
        RunNodeStatus.Pending => "pending",
        RunNodeStatus.Running => "running",
        RunNodeStatus.Succeeded => "done",
        RunNodeStatus.Failed => "fail",
        RunNodeStatus.Waiting => "wait",
        RunNodeStatus.Skipped => "skipped",
        RunNodeStatus.Stopped => "stopped",
        _ => throw new ArgumentOutOfRangeException(nameof(status)),
    };

    public static string Token(this RunOutcome outcome) => outcome switch
    {
        RunOutcome.Running => "running",
        RunOutcome.Succeeded => "done",
        RunOutcome.Failed => "fail",
        RunOutcome.Stopped => "stopped",
        _ => throw new ArgumentOutOfRangeException(nameof(outcome)),
    };

    public static RunNodeStatus ParseStatus(string? token) => token switch
    {
        "pending" => RunNodeStatus.Pending,
        "running" => RunNodeStatus.Running,
        "done" => RunNodeStatus.Succeeded,
        "fail" => RunNodeStatus.Failed,
        "wait" => RunNodeStatus.Waiting,
        "skipped" => RunNodeStatus.Skipped,
        "stopped" => RunNodeStatus.Stopped,
        _ => RunNodeStatus.Pending,
    };

    public static RunOutcome ParseOutcome(string? token) => token switch
    {
        "done" => RunOutcome.Succeeded,
        "fail" => RunOutcome.Failed,
        "stopped" => RunOutcome.Stopped,
        _ => RunOutcome.Running,
    };
}

/// What the client asks the server to dry-run: the process version as its canonical text form
/// (the same bytes a save would send), and the sample data as JSON. A dry run never touches a
/// real request: the server executes the graph against the sample alone.
public sealed record DryRunRequest(
    string ProcessName,
    int Version,
    string TextForm,
    string SampleDataJson);

/// One node of a run snapshot: its state, its attempt and the server's timestamps.
public sealed record RunNodeSnapshot(
    string NodeId,
    RunNodeStatus Status,
    int Attempt = 1,
    DateTimeOffset? StartedAt = null,
    DateTimeOffset? EndedAt = null,
    string? ErrorCode = null,
    string? ErrorMessage = null,
    bool ErrorBranch = false);

/// One snapshot of a dry run: what every node is doing now, and what may happen next. The server
/// decides <see cref="CanStep"/> and <see cref="CanContinue"/>; the client only obeys.
public sealed record RunSnapshot(
    string RunId,
    RunOutcome Outcome,
    IReadOnlyList<RunNodeSnapshot> Nodes,
    bool CanStep = false,
    bool CanContinue = false)
{

    public RunNodeSnapshot? Node(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        foreach (RunNodeSnapshot node in Nodes)
        {
            if (string.Equals(node.NodeId, nodeId, StringComparison.Ordinal))
            {
                return node;
            }
        }

        return null;
    }

    public RunNodeStatus StatusOf(string nodeId) => Node(nodeId)?.Status ?? RunNodeStatus.Pending;
}

/// One row of the run list: enough to group and pick a run without loading it.
public sealed record RunSummary(
    string RunId,
    string ProcessName,
    int Version,
    RunOutcome Outcome,
    DateTimeOffset StartedAt,
    DateTimeOffset? EndedAt = null,
    string? StartedBy = null,
    int FailedNodes = 0)
{

    public TimeSpan? Duration => EndedAt is { } ended ? ended - StartedAt : null;
}

/// One recorded node attempt, with the input it read and the output it produced. Both are JSON
/// text as the server kept them — already redacted with the <c>[redacted:&lt;label&gt;]</c> markers
/// — and the client redacts again before it paints, so a server bug cannot leak a value.
public sealed record RunNodeAttempt(
    string NodeId,
    string Kind,
    int Attempt,
    RunNodeStatus Status,
    DateTimeOffset StartedAt,
    DateTimeOffset? EndedAt = null,
    string? ErrorCode = null,
    string? ErrorMessage = null,
    bool ErrorBranch = false,
    string? InputJson = null,
    string? OutputJson = null)
{

    public TimeSpan? Duration => EndedAt is { } ended ? ended - StartedAt : null;
}

/// One run with its whole node timeline, newest attempt last.
public sealed record RunDetail(RunSummary Summary, IReadOnlyList<RunNodeAttempt> Attempts);

/// Which runs to list: one process and (optionally) one version.
public sealed record RunQuery(string ProcessName, int? Version = null, int Limit = 50, string? Cursor = null);

/// The server refused a run request; <see cref="Code"/> is the contract's error code and the
/// message is already fit to show (the caller's <see cref="RunStrings"/> words it again).
public sealed class RunRefusedException : Exception
{

    public RunRefusedException(string code, string message)
        : base(message) => Code = code;

    public RunRefusedException()
        : this("bpm.run.refused", "The server refused the test run.") { }

    public RunRefusedException(string message)
        : this("bpm.run.refused", message) { }

    public RunRefusedException(string message, Exception inner)
        : base(message, inner) => Code = "bpm.run.refused";

    public string Code { get; } = "bpm.run.refused";
}

/// Everything this project asks the server about runs: start a dry run with sample data, advance
/// it one node or to its end, stop it, list the runs of one process version and load one run's
/// timeline. The contract has none of these endpoints yet (see the task's Open questions); a host
/// binds this to the typed client and the tests bind it to a fake, exactly as
/// <see cref="IFlowDesignerServer"/> does for the designer.
public interface IRunServer
{

    Task<RunSnapshot> StartDryRunAsync(DryRunRequest request, CancellationToken cancellationToken);

    Task<RunSnapshot> StepAsync(string runId, RunStepMode mode, CancellationToken cancellationToken);

    Task<RunSnapshot> StopAsync(string runId, CancellationToken cancellationToken);

    Task<IReadOnlyList<RunSummary>> ListRunsAsync(RunQuery query, CancellationToken cancellationToken);

    Task<RunDetail> LoadRunAsync(string runId, CancellationToken cancellationToken);
}
