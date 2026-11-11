using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.FlowDesigner;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The acceptance scenario of WP-BPM-07: an analyst who never writes code builds
/// webhook → create document → HTTP call → e-mail from the palette with the keyboard alone, runs it
/// with sample data, and reads the history it produced. The saved text form is asserted, because
/// that is what the server would store.
public sealed class ScenarioTests
{

    [AvaloniaFact]

    public void TheThreeSystemFlowIsBuiltWithTheKeyboardAndRunsWithSampleData()
    {
        RunCanvas canvas = NewCanvas(out FlowDocument document);

        AddFromThePalette(canvas, "webh");
        AddFromThePalette(canvas, "create");
        AddFromThePalette(canvas, "http");
        AddFromThePalette(canvas, "mail");

        Assert.Equal(["webhook", "document", "http", "mail"], document.Nodes.Select(node => node.Id));
        Assert.Equal(["webhook", "document", "http", "mail"], document.Nodes.Select(node => node.Kind));
        Assert.Equal("Webhook", canvas.Catalogue.DisplayName("webhook"));

        ConnectWithTheKeyboard(canvas, "webhook", "document");
        ConnectWithTheKeyboard(canvas, "document", "http");
        ConnectWithTheKeyboard(canvas, "http", "mail");

        Assert.Equal(
            [new FlowEdgeKey("webhook", "document"), new FlowEdgeKey("document", "http"), new FlowEdgeKey("http", "mail")],
            document.Edges.Select(edge => edge.Key));

        // The run: the same document, the sample data, and a server that walks the four nodes.
        FakeRunServer server = new() { Detail = ScenarioRun() };
        server.Enqueue(RunFixtures.Snapshot("run_5", RunOutcome.Running,
            ("webhook", RunNodeStatus.Succeeded), ("document", RunNodeStatus.Running),
            ("http", RunNodeStatus.Pending), ("mail", RunNodeStatus.Pending)));
        server.Enqueue(RunFixtures.Snapshot("run_5", RunOutcome.Running,
            ("webhook", RunNodeStatus.Succeeded), ("document", RunNodeStatus.Succeeded),
            ("http", RunNodeStatus.Running), ("mail", RunNodeStatus.Pending)));
        server.Enqueue(RunFixtures.Snapshot("run_5", RunOutcome.Running,
            ("webhook", RunNodeStatus.Succeeded), ("document", RunNodeStatus.Succeeded),
            ("http", RunNodeStatus.Succeeded), ("mail", RunNodeStatus.Running)));
        server.Enqueue(RunFixtures.Snapshot("run_5", RunOutcome.Succeeded,
            ("webhook", RunNodeStatus.Succeeded), ("document", RunNodeStatus.Succeeded),
            ("http", RunNodeStatus.Succeeded), ("mail", RunNodeStatus.Succeeded)));

        FlowRunSession session = new(server, document, canvas.Catalogue);
        canvas.Session = session;
        session.SampleData.Text = RunFixtures.SampleJson;

        TestHost.Await(session.StartTestAsync(TestContext.Current.CancellationToken));
        Assert.Equal("Running", canvas.BadgeText("webhook"));
        Assert.Equal("Pending", canvas.BadgeText("mail"));

        while (session.CanContinue)
        {
            TestHost.Await(session.ContinueAsync(TestContext.Current.CancellationToken));
        }

        Assert.Equal(RunOutcome.Succeeded, session.Snapshot!.Outcome);
        Assert.Equal(4, server.Steps.Count);
        Assert.All(server.Steps, step => Assert.Equal(RunStepMode.Continue, step.Mode));
        Assert.All(canvas.Badges, badge => Assert.Equal(RunNodeStatus.Succeeded, badge.Status));

        // The history the analyst reads: four nodes, in order, with their input and output panes.
        Assert.Equal(4, session.History.Timeline.Count);
        Assert.Equal(["webhook", "document", "http", "mail"], session.History.Timeline.Select(row => row.NodeId));
        Assert.StartsWith("Request run_5", session.History.Heading, StringComparison.Ordinal);
        Assert.All(session.History.Timeline, row => Assert.Equal("Succeeded", row.StatusText));

        string history = session.History.ToText();
        Assert.Contains("1. webhook (Webhook): Succeeded, attempt 1", history, StringComparison.Ordinal);
        Assert.Contains("4. mail (Send e-mail): Succeeded, attempt 1", history, StringComparison.Ordinal);
        Assert.Contains("A-1", session.History.Exchange("document", 1)!.InputText, StringComparison.Ordinal);

        // The saved text form: exactly what the server received, and a round trip.
        string saved = document.ToText();
        Assert.Equal(saved, server.Started[0].TextForm, StringComparer.Ordinal);
        FlowDocument reread = FlowDocument.FromText(saved);
        Assert.Equal(["webhook", "document", "http", "mail"], reread.Nodes.Select(node => node.Id));
        Assert.Equal(["webhook", "document", "http", "mail"], reread.Nodes.Select(node => node.Kind));
        Assert.Equal(document.Edges.Select(edge => edge.Key), reread.Edges.Select(edge => edge.Key));
        Assert.Contains("id = \"webhook\"", saved, StringComparison.Ordinal);
        Assert.Contains("kind = \"webhook\"", saved, StringComparison.Ordinal);
        Assert.Contains("from = \"webhook\"", saved, StringComparison.Ordinal);
        Assert.Contains("to = \"mail\"", saved, StringComparison.Ordinal);
        Assert.Equal(3, saved.Split("[[edges]]").Length - 1);
    }

    [AvaloniaFact]

    public void TheKeyboardWalkNeverNeedsAPointer()
    {
        RunCanvas canvas = NewCanvas(out FlowDocument document);

        AddFromThePalette(canvas, "webh");
        AddFromThePalette(canvas, "mail");

        Assert.Equal(2, document.Nodes.Count);
        Assert.True(canvas.IsTypeAheadOpen == false);

        // Escape drops the selection, so the arrows walk the nodes again rather than moving one.
        canvas.HandleKey(Key.Escape, KeyModifiers.None);
        Assert.True(canvas.Selection.IsEmpty);

        Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        Assert.Equal("webhook", canvas.Selection.SingleNodeId);
    }

    private static RunCanvas NewCanvas(out FlowDocument document)
    {
        document = RunFixtures.Document();
        RunCanvas canvas = new()
        {
            Catalogue = RunFixtures.Catalogue(),
            Document = document,
        };
        TestHost.Show(canvas, 1024, 700);
        canvas.FitToContent();
        TestHost.Pump();
        return canvas;
    }

    /// A adds a node by type-ahead: the query and Enter, nothing else.
    private static void AddFromThePalette(RunCanvas canvas, string query)
    {
        Assert.True(canvas.HandleKey(Key.A, KeyModifiers.None));
        canvas.TypeTypeAhead(query);
        Assert.True(canvas.CommitTypeAhead());
        TestHost.Pump();
    }

    /// Selects the source with the arrows, starts a connection with C, walks the candidates with
    /// the arrow keys and completes it with Enter.
    private static void ConnectWithTheKeyboard(RunCanvas canvas, string from, string to)
    {
        canvas.HandleKey(Key.Escape, KeyModifiers.None);
        while (!string.Equals(canvas.Selection.SingleNodeId, from, StringComparison.Ordinal))
        {
            Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        }

        Assert.True(canvas.HandleKey(Key.C, KeyModifiers.None));
        for (int guard = 0; guard < 16 && !string.Equals(canvas.ConnectCandidate, to, StringComparison.Ordinal); guard++)
        {
            Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        }

        Assert.Equal(to, canvas.ConnectCandidate);
        Assert.True(canvas.HandleKey(Key.Enter, KeyModifiers.None));
        Assert.Null(canvas.ConnectFrom);
        TestHost.Pump();
    }

    /// The run the fake server records for the flow the keyboard just built.
    private static RunDetail ScenarioRun()
    {
        DateTimeOffset start = RunFixtures.T0;
        RunSummary summary = new("run_5", "order_intake", 1, RunOutcome.Succeeded, start, start.AddSeconds(2), "ana");
        List<RunNodeAttempt> attempts =
        [
            new("webhook", "webhook", 1, RunNodeStatus.Succeeded, start, start.AddMilliseconds(100),
                InputJson: """{"path":"/hooks/orders"}""",
                OutputJson: """{"order":{"id":"A-1"}}"""),
            new("document", "document", 1, RunNodeStatus.Succeeded, start.AddMilliseconds(100), start.AddMilliseconds(800),
                InputJson: """{"id":"A-1","doctype":"Sales Order"}""",
                OutputJson: """{"name":"SO-1"}"""),
            new("http", "http", 1, RunNodeStatus.Succeeded, start.AddMilliseconds(800), start.AddMilliseconds(1600),
                InputJson: """{"url":"https://api.test/orders","token":"[redacted:api]"}""",
                OutputJson: """{"accepted":true}"""),
            new("mail", "mail", 1, RunNodeStatus.Succeeded, start.AddMilliseconds(1600), start.AddSeconds(2),
                InputJson: """{"to":["ops@acme.test"]}""",
                OutputJson: """{"message_id":"m-1"}"""),
        ];

        return new RunDetail(summary, attempts);
    }
}
