using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Tests;

/// Shared fixtures: the server's kind catalogue, a fake server and a canvas on a window.
public static class FlowFixtures
{
    /// The document of a shipped template, read from its golden text.
    public static FlowDocument Document(string name = "order_to_cash.toml") =>
        FlowDocument.FromText(TestHost.Golden(name + ".golden"));
    /// The kinds a server would send, with the field types the panels render.
    public static FlowKindCatalogue Catalogue() => new(
    [
        new FlowKindSchema("trigger", "Trigger", "Start", [
            new FlowKindField("trigger", "Starts on", FlowFieldType.Select, Required: true,
                Options: ["manual", "document_event", "schedule", "webhook", "api"], DefaultValue: FlowValue.Text("manual")),
            new FlowKindField("doctype", "Document type", FlowFieldType.Text),
            new FlowKindField("event", "Lifecycle event", FlowFieldType.Text),
            new FlowKindField("cron", "Schedule", FlowFieldType.Text),
            new FlowKindField("path", "Path", FlowFieldType.Text),
        ]),
        new FlowKindSchema("document", "Document", "Work", [
            new FlowKindField("action", "Action", FlowFieldType.Select, Required: true,
                Options: ["create", "update", "post", "cancel", "map"], DefaultValue: FlowValue.Text("create")),
            new FlowKindField("doctype", "Document type", FlowFieldType.Text, Required: true),
            new FlowKindField("operation", "Lifecycle operation", FlowFieldType.Text),
            new FlowKindField("mapping", "Mapping", FlowFieldType.Expression),
        ], ExpressionFields: ["total", "approved"]),
        new FlowKindSchema("decision", "Decision", "People", [
            new FlowKindField("title", "Question", FlowFieldType.Text, Required: true),
            new FlowKindField("assignee", "Who decides", FlowFieldType.Text),
            new FlowKindField("outcomes", "Outcomes", FlowFieldType.StringList),
        ]),
        new FlowKindSchema("integration", "Integration", "Work", [
            new FlowKindField("connector", "Connector", FlowFieldType.Text, Required: true),
            new FlowKindField("action", "Action", FlowFieldType.Text, Required: true),
            new FlowKindField("endpoint", "Endpoint", FlowFieldType.Text),
        ]),
        new FlowKindSchema("logic", "Logic", "Work", [
            new FlowKindField("logic", "Kind of logic", FlowFieldType.Select,
                Options: ["branch", "parallel", "join", "loop", "map", "script"], DefaultValue: FlowValue.Text("branch")),
            new FlowKindField("script", "Script", FlowFieldType.MultilineText),
        ]),
        new FlowKindSchema("wait", "Wait", "Work", [
            new FlowKindField("wait", "Waits for", FlowFieldType.Select, Options: ["event", "date", "duration"], DefaultValue: FlowValue.Text("event")),
            new FlowKindField("event", "Event", FlowFieldType.Text),
            new FlowKindField("seconds", "Seconds", FlowFieldType.WholeNumber),
            new FlowKindField("until", "Until", FlowFieldType.Text),
        ]),
        new FlowKindSchema("sub_process", "Sub-process", "Work", [
            new FlowKindField("process", "Process", FlowFieldType.Text, Required: true),
            new FlowKindField("version", "Version", FlowFieldType.WholeNumber),
        ]),
        new FlowKindSchema("agent", "Agent", "Work", [
            new FlowKindField("tier", "Tier", FlowFieldType.Text, Required: true),
            new FlowKindField("instruction", "Instruction", FlowFieldType.MultilineText),
            new FlowKindField("confidence", "Confidence rule", FlowFieldType.Expression),
        ], ExpressionFields: ["total", "approved"]),
    ]);
    /// A canvas over a document, shown in a headless window and laid out.
    public static FlowCanvas Canvas(FlowDocument? document = null, double width = 900, double height = 640)
    {
        FlowCanvas canvas = new()
        {
            Catalogue = Catalogue(),
            Document = document ?? FlowDocument.FromText(TestHost.Golden("order_to_cash.toml.golden")),
        };
        TestHost.Show(canvas, width, height);
        canvas.FitToContent();
        TestHost.Pump();
        return canvas;
    }
    /// The screen point in the middle of a node.
    public static Avalonia.Point Centre(FlowCanvas canvas, string id)
    {
        ArgumentNullException.ThrowIfNull(canvas);
        FlowNode node = canvas.Document?.Node(id) ?? throw new ArgumentException("No node " + id, nameof(id));
        return canvas.ToScreen(new FlowPoint(
            node.Position.X + (FlowLayoutEngine.NodeWidth / 2),
            node.Position.Y + (FlowLayoutEngine.NodeHeight / 2)));
    }
}

/// The server fake: it records every question and answers with a script the test sets. A pending
/// mode holds the answer on a task the test completes, which is how the stale-result test wins the
/// race on purpose.
public sealed class FakeServer : IFlowDesignerServer
{
    /// Every text form sent for validation, in order.
    public List<string> Validated { get; } = [];
    /// Every text form saved, in order.
    public List<string> Saved { get; } = [];
    /// Every text form sent for publication, in order.
    public List<string> Published { get; } = [];
    /// The report the server answers a validation with.
    public Func<string, FlowValidationReport> Report { get; set; } =
        _ => new FlowValidationReport([], 1);
    /// The versions the server lists.
    public List<FlowVersionSummary> VersionRows { get; } = [];
    /// When set, a validation waits for this task before answering.
    public TaskCompletionSource<FlowValidationReport>? Pending { get; set; }
    /// The version the next save creates.
    public int NextVersion { get; set; } = 7;

    public Task<FlowKindCatalogue> LoadKindsAsync(CancellationToken cancellationToken) =>
        Task.FromResult(FlowFixtures.Catalogue());

    public Task<FlowValidationReport> ValidateAsync(string textForm, CancellationToken cancellationToken)
    {
        Validated.Add(textForm);
        return Pending is { } pending ? pending.Task : Task.FromResult(Report(textForm));
    }

    public Task<FlowDraftSaved> SaveDraftAsync(string textForm, string? expectedContentHash, CancellationToken cancellationToken)
    {
        Saved.Add(textForm);
        ExpectedHash = expectedContentHash;
        return Task.FromResult(new FlowDraftSaved(NextVersion, "hash-" + NextVersion.ToString(System.Globalization.CultureInfo.InvariantCulture)));
    }

    public Task<FlowPublishAccepted> RequestPublishAsync(string textForm, CancellationToken cancellationToken)
    {
        Published.Add(textForm);
        return Task.FromResult(new FlowPublishAccepted("review-1"));
    }

    public Task<IReadOnlyList<FlowVersionSummary>> ListVersionsAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<FlowVersionSummary>>(VersionRows);
    /// The hash the last save was given, for the conflict test.
    public string? ExpectedHash { get; private set; }
}
