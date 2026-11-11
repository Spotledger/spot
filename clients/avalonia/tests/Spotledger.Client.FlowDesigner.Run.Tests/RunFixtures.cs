using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The server's kind catalogue for a three-system flow, and a fake run server whose answers each
/// test scripts. The fake records every question, so a test can prove what the client sent as well
/// as what it showed.
public static class RunFixtures
{

    /// A value long enough for the redactor to match (spot-redact's eight-byte bound).
    public const string Canary = "canary-credential-0123456789-abcdef";

    /// A fixed instant: every timestamp in a test is this plus an offset, never a clock reading.
    public static DateTimeOffset T0 => new(2026, 10, 20, 9, 30, 0, TimeSpan.Zero);

    /// The kinds a server would offer for the flow the brief names.
    public static FlowKindCatalogue Catalogue() => new(
    [
        new FlowKindSchema("webhook", "Webhook", "Start", [
            new FlowKindField("trigger", "Starts on", FlowFieldType.Select, Required: true,
                Options: ["webhook", "manual"], DefaultValue: FlowValue.Text("webhook")),
            new FlowKindField("path", "Path", FlowFieldType.Text, Required: true),
        ]),
        new FlowKindSchema("document", "Create document", "Work", [
            new FlowKindField("action", "Action", FlowFieldType.Select, Required: true,
                Options: ["create", "update"], DefaultValue: FlowValue.Text("create")),
            new FlowKindField("doctype", "Document type", FlowFieldType.Text, Required: true),
        ]),
        new FlowKindSchema("http", "HTTP call", "Work", [
            new FlowKindField("url", "URL", FlowFieldType.Text, Required: true),
            new FlowKindField("method", "Method", FlowFieldType.Select, Required: true,
                Options: ["GET", "POST"], DefaultValue: FlowValue.Text("POST")),
        ]),
        new FlowKindSchema("mail", "Send e-mail", "Work", [
            new FlowKindField("to", "To", FlowFieldType.Text, Required: true),
            new FlowKindField("subject", "Subject", FlowFieldType.Text),
        ]),
    ]);

    /// An empty process the scenario test fills from the palette.
    public static FlowDocument Document(string name = "order_intake") => new(name, "acme");

    /// The four-node flow as the server's text form, for the tests that need a finished process
    /// without driving the canvas: webhook, create document, HTTP call, e-mail.
    public static FlowDocument ScenarioDocument() => FlowDocument.FromText(
        """
        # Spotledger process definition (spot-bpm-model). Canonical: keys sorted, one scalar per line.
        name = "order_intake"
        app = "acme"
        version = 3
        status = "published"

        [[nodes]]
        id = "webhook"
        kind = "webhook"
        path = "/hooks/orders"

        [[nodes]]
        id = "document"
        kind = "document"
        doctype = "Sales Order"

        [[nodes]]
        id = "http"
        kind = "http"
        url = "https://api.test/orders"

        [[nodes]]
        id = "mail"
        kind = "mail"
        to = "ops@acme.test"

        [[edges]]
        from = "webhook"
        to = "document"

        [[edges]]
        from = "document"
        to = "http"

        [[edges]]
        from = "http"
        to = "mail"
        """);

    /// The sample data a webhook trigger delivers.
    public const string SampleJson =
        """
        {"path":"/hooks/orders","payload":{"order":{"id":"A-1","total":"19.90"}},"headers":{"content-type":"application/json"}}
        """;

    /// A snapshot with the given per-node states; a running run may be stepped or continued.
    public static RunSnapshot Snapshot(string runId, RunOutcome outcome, params (string Node, RunNodeStatus Status)[] nodes)
    {
        ArgumentNullException.ThrowIfNull(nodes);
        List<RunNodeSnapshot> list = [];
        foreach ((string node, RunNodeStatus status) in nodes)
        {
            list.Add(new RunNodeSnapshot(node, status));
        }

        bool running = outcome == RunOutcome.Running;
        return new RunSnapshot(runId, outcome, list, CanStep: running, CanContinue: running);
    }

    /// A run of the four-node flow: the HTTP call fails once, retries, then succeeds.
    public static RunDetail ScenarioDetail()
    {
        RunSummary summary = new(
            "run_7",
            "order_intake",
            3,
            RunOutcome.Failed,
            T0,
            T0.AddSeconds(3),
            "ana",
            FailedNodes: 1);

        List<RunNodeAttempt> attempts =
        [
            new("webhook", "webhook", 1, RunNodeStatus.Succeeded, T0, T0.AddMilliseconds(120),
                InputJson: """{"path":"/hooks/orders"}""",
                OutputJson: """{"order":{"id":"A-1"}}"""),
            new("document", "document", 1, RunNodeStatus.Succeeded, T0.AddMilliseconds(120), T0.AddMilliseconds(700),
                InputJson: """{"doctype":"Sales Order","token":"[redacted:api]"}""",
                OutputJson: """{"name":"SO-1"}"""),
            new("http", "http", 1, RunNodeStatus.Failed, T0.AddMilliseconds(700), T0.AddMilliseconds(1700),
                ErrorCode: "bpm_integration.http_status",
                ErrorMessage: "HTTP 500 from the peer",
                ErrorBranch: true,
                InputJson: """{"url":"https://api.test/orders","authorization":"[redacted:header]"}"""),
            new("http", "http", 2, RunNodeStatus.Succeeded, T0.AddMilliseconds(1700), T0.AddMilliseconds(2600),
                InputJson: """{"url":"https://api.test/orders"}""",
                OutputJson: """{"accepted":true}"""),
            new("mail", "mail", 1, RunNodeStatus.Succeeded, T0.AddMilliseconds(2600), T0.AddSeconds(3),
                InputJson: """{"to":["ops@acme.test"],"subject":"Order A-1"}""",
                OutputJson: """{"message_id":"m-1"}"""),
        ];

        return new RunDetail(summary, attempts);
    }

    /// The run list of two process versions, so grouping is visible.
    public static IReadOnlyList<RunSummary> Runs() =>
    [
        new("run_9", "order_intake", 4, RunOutcome.Running, T0.AddMinutes(10)),
        new("run_7", "order_intake", 3, RunOutcome.Failed, T0, T0.AddSeconds(3), "ana", 1),
        new("run_8", "order_intake", 3, RunOutcome.Succeeded, T0.AddMinutes(5), T0.AddMinutes(6), "bo"),
        new("run_6", "invoice_check", 2, RunOutcome.Stopped, T0.AddMinutes(-30)),
    ];
}

/// The run server fake: it answers with the script the test sets and records every question.
public sealed class FakeRunServer : IRunServer
{

    private readonly Queue<RunSnapshot> _advance = new();

    /// Every dry-run request, in order.
    public List<DryRunRequest> Started { get; } = [];

    /// Every step, in order: the run and the mode.
    public List<(string RunId, RunStepMode Mode)> Steps { get; } = [];

    /// Every stop, in order.
    public List<string> Stops { get; } = [];

    /// Every run query, in order.
    public List<RunQuery> Queries { get; } = [];

    /// The run list the server answers with.
    public IReadOnlyList<RunSummary> Runs { get; set; } = [];

    /// The run the server opens on demand.
    public RunDetail? Detail { get; set; }

    /// The snapshot a start answers with; the default is a fresh running run.
    public RunSnapshot? StartResult { get; set; }

    /// The snapshot a stop answers with.
    public RunSnapshot? StopResult { get; set; }

    /// When set, a start fails with this refusal (a server that says no).
    public RunRefusedException? StartFault { get; set; }

    /// When set, a step fails with this refusal.
    public RunRefusedException? StepFault { get; set; }

    /// How many steps the script still holds.
    public int ScriptedSteps => _advance.Count;

    /// Queues one answer for the next step.
    public void Enqueue(RunSnapshot snapshot)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        _advance.Enqueue(snapshot);
    }

    public Task<RunSnapshot> StartDryRunAsync(DryRunRequest request, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(request);
        Started.Add(request);
        if (StartFault is { } fault)
        {
            throw fault;
        }

        return Task.FromResult(StartResult ?? RunFixtures.Snapshot(
            "run_1",
            RunOutcome.Running,
            ("webhook", RunNodeStatus.Running),
            ("document", RunNodeStatus.Pending),
            ("http", RunNodeStatus.Pending),
            ("mail", RunNodeStatus.Pending)));
    }

    public Task<RunSnapshot> StepAsync(string runId, RunStepMode mode, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(runId);
        Steps.Add((runId, mode));
        if (StepFault is { } fault)
        {
            throw fault;
        }

        return Task.FromResult(_advance.Count > 0
            ? _advance.Dequeue()
            : RunFixtures.Snapshot(runId, RunOutcome.Succeeded, ("webhook", RunNodeStatus.Succeeded)));
    }

    public Task<RunSnapshot> StopAsync(string runId, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(runId);
        Stops.Add(runId);
        return Task.FromResult(StopResult ?? RunFixtures.Snapshot(runId, RunOutcome.Stopped, ("webhook", RunNodeStatus.Stopped)));
    }

    public Task<IReadOnlyList<RunSummary>> ListRunsAsync(RunQuery query, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(query);
        Queries.Add(query);
        return Task.FromResult(Runs);
    }

    public Task<RunDetail> LoadRunAsync(string runId, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(runId);
        RunDetail detail = Detail ?? throw new InvalidOperationException("No detail is scripted.");
        return Task.FromResult(detail);
    }
}
