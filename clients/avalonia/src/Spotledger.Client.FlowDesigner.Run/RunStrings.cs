using System;
using System.Collections.Generic;
using System.Globalization;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run;

/// Every user-visible string this project can produce, in one place (ADR-073, ADR-080).
///
/// Nothing here names a run with the internal word: the name of a run is the organisation's label
/// pair, <c>bpm.run.singular</c> / <c>bpm.run.plural</c> (default "Request"/"Requests"), read from
/// <see cref="Labels"/>. Code, events and error codes keep the internal vocabulary; a person never
/// reads it. <see cref="All"/> lists every string for the scan test, which fails when the internal
/// word reaches a person.
public sealed class RunStrings
{

    public RunStrings(RunLabels? labels = null) => Labels = labels ?? RunLabels.Defaults;

    public RunLabels Labels { get; }

    public static string Title => "Test run and history";

    public static string WorkspaceName => "Test run workspace";

    public static string SampleDataName => "Sample data";

    public static string SampleDataHelp =>
        "The JSON a trigger would send. The trigger's own settings are listed as hints.";

    public static string SampleDataHintName => "Trigger hints";

    public static string RunTest => "Run test";

    public static string Continue => "Continue";

    public static string Step => "Step";

    public static string Stop => "Stop";

    public static string HistoryName => "Run history";

    public static string HistoryHeading => "History";

    public static string TimelineHeading => "Node timeline";

    public static string StatusName => "Node states";

    public static string InputPane => "Input";

    public static string OutputPane => "Output";

    public static string ErrorLabel => "Error";

    public static string Redacted => "[redacted]";

    public static string RedactedWith(string label) => "[redacted:" + label + "]";

    public static string RedactionNote =>
        "A credential never renders: sensitive keys and known values show the server's marker.";

    public static string NoInput => "(no input)";

    public static string NoOutput => "(no output)";

    public static string ErrorBranch => "error branch";

    public static string Retry => "retry";

    public static string Pending => "Pending";

    public static string Running => "Running";

    public static string Succeeded => "Succeeded";

    public static string Failed => "Failed";

    public static string Waiting => "Waiting";

    public static string Skipped => "Skipped";

    public static string Stopped => "Stopped";

    public string NoRuns => "No " + Labels.PluralLower + " yet. Run a test to see the history.";

    public string NoTimeline => "Pick a " + Labels.SingularLower + " to see its nodes.";

    public string GroupHeading(string process, int version) =>
        Labels.Plural + " of " + process + ", version " + version.ToString(CultureInfo.InvariantCulture);

    public string RunHeading(RunSummary summary)
    {
        ArgumentNullException.ThrowIfNull(summary);
        return Labels.Singular + " " + summary.RunId + ", " + OutcomeText(summary.Outcome)
            + ", started " + summary.StartedAt.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture)
            + " UTC";
    }

    public string TestStarted(string runId) => "Test " + Labels.SingularLower + " " + runId + " started.";

    public static string TestStepped(string? nodeId) =>
        nodeId is null
            ? "Stepped one node."
            : "Stepped to node " + nodeId + ".";

    public string TestFinished(RunOutcome outcome) =>
        "Test " + Labels.SingularLower + " finished: " + OutcomeText(outcome) + ".";

    public string TestStopped => "Test " + Labels.SingularLower + " stopped.";

    public string StillRunning => "That test " + Labels.SingularLower + " is still running.";

    public static string NothingToStep => "The server has no next step to take.";

    public static string NoRunYet => "Run a test first.";

    public static string SampleDataNotObject => "Sample data must be a JSON object, so the trigger's fields can be read.";

    public static string SampleDataInvalid => "Sample data is not valid JSON.";

    public static string SampleDataAccepted => "Sample data is ready for a test run.";

    public static string SampleDataUnknownKeys(int count) =>
        count == 1
            ? "1 key is not part of the trigger; it is kept as written."
            : count.ToString(CultureInfo.InvariantCulture)
                + " keys are not part of the trigger; they are kept as written.";

    public static string SampleDataReason(string reason) => "The JSON reader said: " + reason;

    /// A command's accessible help: its name and the key that runs it.
    public static string Shortcut(string command, string key) => command + " (" + key + ")";

    public static string SampleDataInvalidAt(long line, long column) =>
        "Sample data is not valid JSON at line " + line.ToString(CultureInfo.InvariantCulture)
        + ", column " + column.ToString(CultureInfo.InvariantCulture) + ".";

    public static string UnknownKey(string key) =>
        "The trigger does not deliver " + key + "; the value is kept as written.";

    public static string HintPayload => "payload: the body the trigger delivers";

    public static string HintHeaders => "headers: the request headers the trigger saw";

    public static string HintRequired(string field) => field + " (required by the trigger)";

    public static string HintOptional(string field) => field + " (optional)";

    public static string StepRefused(string reason) => "Cannot step: " + reason;

    public static string StatusText(RunNodeStatus status) => status switch
    {
        RunNodeStatus.Pending => Pending,
        RunNodeStatus.Running => Running,
        RunNodeStatus.Succeeded => Succeeded,
        RunNodeStatus.Failed => Failed,
        RunNodeStatus.Waiting => Waiting,
        RunNodeStatus.Skipped => Skipped,
        RunNodeStatus.Stopped => Stopped,
        _ => throw new ArgumentOutOfRangeException(nameof(status)),
    };

    /// The word on a node badge: short enough for the canvas, the same words the status list uses.
    public static string BadgeText(RunNodeStatus status) => StatusText(status);

    public static string OutcomeText(RunOutcome outcome) => outcome switch
    {
        RunOutcome.Running => "running",
        RunOutcome.Succeeded => "succeeded",
        RunOutcome.Failed => "failed",
        RunOutcome.Stopped => "stopped",
        _ => throw new ArgumentOutOfRangeException(nameof(outcome)),
    };

    public static string AttemptText(int attempt) =>
        "attempt " + attempt.ToString(CultureInfo.InvariantCulture);

    public static string AttemptsText(int attempts) =>
        attempts == 1 ? "1 attempt" : attempts.ToString(CultureInfo.InvariantCulture) + " attempts";

    public static string BadgeHelp(string nodeId, RunNodeStatus status, int attempt) =>
        "Node " + nodeId + ": " + StatusText(status) + ", " + AttemptText(attempt);

    public static string DurationText(TimeSpan duration)
    {
        long millis = duration.Ticks <= 0 ? 0 : duration.Ticks / TimeSpan.TicksPerMillisecond;
        if (millis < 1000)
        {
            return millis.ToString(CultureInfo.InvariantCulture) + " ms";
        }

        long seconds = millis / 1000;
        if (seconds < 60)
        {
            return seconds.ToString(CultureInfo.InvariantCulture)
                + "." + ((millis % 1000) / 100).ToString(CultureInfo.InvariantCulture) + " s";
        }

        long minutes = seconds / 60;
        long rest = seconds % 60;
        return minutes.ToString(CultureInfo.InvariantCulture)
            + " min " + rest.ToString("00", CultureInfo.InvariantCulture) + " s";
    }

    public static string DurationLabel(TimeSpan? duration) =>
        duration is { } value ? DurationText(value) : "—";

    /// Every string this project can show, so the ADR-080 scan sees the whole surface at once.
    public IReadOnlyList<string> All =>
    [
        Title,
        WorkspaceName,
        SampleDataName,
        SampleDataHelp,
        SampleDataHintName,
        RunTest,
        Continue,
        Step,
        Stop,
        HistoryName,
        HistoryHeading,
        TimelineHeading,
        StatusName,
        InputPane,
        OutputPane,
        ErrorLabel,
        Redacted,
        RedactedWith("api"),
        RedactionNote,
        NoInput,
        NoOutput,
        ErrorBranch,
        Retry,
        Pending,
        Running,
        Succeeded,
        Failed,
        Waiting,
        Skipped,
        Stopped,
        NoRuns,
        NoTimeline,
        GroupHeading("order_intake", 3),
        RunHeading(new RunSummary(
            "run_7",
            "order_intake",
            3,
            RunOutcome.Succeeded,
            new DateTimeOffset(2026, 10, 20, 9, 30, 0, TimeSpan.Zero),
            new DateTimeOffset(2026, 10, 20, 9, 30, 2, TimeSpan.Zero),
            "ana",
            0)),
        TestStarted("run_7"),
        TestStepped("create_document"),
        TestStepped(null),
        TestFinished(RunOutcome.Failed),
        TestStopped,
        StillRunning,
        NothingToStep,
        NoRunYet,
        SampleDataNotObject,
        SampleDataInvalid,
        SampleDataAccepted,
        SampleDataUnknownKeys(2),
        SampleDataReason("an unclosed object."),
        Shortcut(RunTest, "F5"),
        SampleDataInvalidAt(3, 14),
        UnknownKey("customer"),
        HintPayload,
        HintHeaders,
        HintRequired("path"),
        HintOptional("doctype"),
        StepRefused("the server is busy."),
        StatusText(RunNodeStatus.Failed),
        BadgeText(RunNodeStatus.Waiting),
        OutcomeText(RunOutcome.Stopped),
        AttemptText(2),
        AttemptsText(2),
        BadgeHelp("http_call", RunNodeStatus.Succeeded, 2),
        DurationText(TimeSpan.FromMilliseconds(820)),
        DurationText(TimeSpan.FromMilliseconds(1240)),
        DurationText(TimeSpan.FromSeconds(125)),
        DurationLabel(null),
        DurationLabel(TimeSpan.FromMilliseconds(7)),
    ];
}
