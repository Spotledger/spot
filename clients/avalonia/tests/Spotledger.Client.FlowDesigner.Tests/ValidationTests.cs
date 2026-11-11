using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// Validation is debounced, sent to the server and generation-checked: an edit waits out the
/// window, one check goes for a burst of edits, a slow answer for an older edit is dropped, and the
/// problems the server returns land on the nodes (so a problem click can focus one). The client's
/// own advice is a hint and never an error.
public sealed class ValidationTests
{

    private static readonly TimeSpan Debounce = TimeSpan.FromMilliseconds(250);

    private static (FlowValidationCoordinator Coordinator, FakeServer Server, FlowEditor Editor) Make(
        Func<string, FlowValidationReport>? report = null)
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document);
        FakeServer server = new();
        if (report is not null)
        {
            server.Report = report;
        }

        return (new FlowValidationCoordinator(server, editor, Debounce), server, editor);
    }

    [Fact]

    public async Task AnEditWaitsOutTheDebounceWindowBeforeOneCheckGoes()
    {
        (FlowValidationCoordinator coordinator, FakeServer server, FlowEditor editor) = Make();
        editor.Execute(new SetTitleCommand("changed"));
        coordinator.MarkEdited(TimeSpan.FromMilliseconds(1000));

        Assert.False(await coordinator.PollAsync(TimeSpan.FromMilliseconds(1100), CancellationToken.None));
        Assert.Empty(server.Validated);
        Assert.True(coordinator.IsDirty);

        Assert.True(await coordinator.PollAsync(TimeSpan.FromMilliseconds(1300), CancellationToken.None));
        Assert.Single(server.Validated);
        Assert.Equal(editor.Document.ToText(), server.Validated[0], StringComparer.Ordinal);
        Assert.False(coordinator.IsDirty);
    }

    [Fact]

    public async Task ABurstOfEditsSendsOneCheckAndTheLatestText()
    {
        (FlowValidationCoordinator coordinator, FakeServer server, FlowEditor editor) = Make();
        for (int i = 0; i < 5; i++)
        {
            editor.Execute(new SetTitleCommand("title " + i.ToString(System.Globalization.CultureInfo.InvariantCulture)));
            coordinator.MarkEdited(TimeSpan.FromMilliseconds(1000 + (i * 10)));
        }

        Assert.False(await coordinator.PollAsync(TimeSpan.FromMilliseconds(1010), CancellationToken.None));
        Assert.True(await coordinator.PollAsync(TimeSpan.FromMilliseconds(1400), CancellationToken.None));

        Assert.Single(server.Validated);
        Assert.Contains("title 4", server.Validated[0], StringComparison.Ordinal);
    }

    [Fact]

    public async Task ASlowAnswerForAnOlderEditIsDropped()
    {
        (FlowValidationCoordinator coordinator, FakeServer server, FlowEditor editor) = Make();
        TaskCompletionSource<FlowValidationReport> pending = new();
        server.Pending = pending;
        editor.Execute(new SetTitleCommand("first"));
        coordinator.MarkEdited(TimeSpan.FromMilliseconds(1000));

        Task<IReadOnlyList<FlowProblem>> slow = coordinator.ValidateNowAsync(CancellationToken.None);
        editor.Execute(new SetTitleCommand("second"));
        coordinator.MarkEdited(TimeSpan.FromMilliseconds(2000));
        pending.SetResult(new FlowValidationReport([new FlowProblem("bpm.x", "stale", FlowProblemSeverity.Error)], 1));

        Assert.Empty(await slow);
        Assert.Empty(coordinator.Problems);
    }

    [Fact]

    public async Task TheServersProblemsLandOnNodesAndInTheList()
    {
        (FlowValidationCoordinator coordinator, _, _) = Make(_ => new FlowValidationReport(
        [
            new FlowProblem("bpm_model.unknown_node_kind", "The kind is newer than this build.", FlowProblemSeverity.Error, "approve"),
            new FlowProblem("bpm_model.join_unreachable", "The join can never complete.", FlowProblemSeverity.Warning, "post_delivery"),
        ], 4));

        coordinator.MarkEdited(TimeSpan.Zero);
        await coordinator.PollAsync(TimeSpan.FromMilliseconds(300), CancellationToken.None);

        Assert.Equal(4, coordinator.AcceptedRevision);
        Assert.Equal(2, coordinator.Problems.Count(problem => !problem.IsHint));
        Assert.Single(coordinator.ForNode("approve"));
        Assert.Equal("bpm_model.unknown_node_kind", coordinator.WorstFor("approve")!.Code);
        Assert.Null(coordinator.WorstFor("start"));
    }

    [Fact]

    public async Task LocalAdviceIsAHintAndNeverAnError()
    {
        FlowDocument document = new("demo", "demo");
        document.AddNode(new FlowNode("lonely", "logic"));
        document.AddNode(new FlowNode("other", "logic"));
        FlowEditor editor = new(document);
        FlowValidationCoordinator coordinator = new(new FakeServer(), editor, Debounce);

        coordinator.MarkEdited(TimeSpan.Zero);
        IReadOnlyList<FlowProblem> problems = await coordinator.ValidateNowAsync(CancellationToken.None);

        Assert.All(problems, problem => Assert.True(problem.IsHint));
        Assert.Contains(problems, problem => problem.Code == "hint.no_trigger");
        Assert.Contains(problems, problem => problem.Code == "hint.unconnected" && problem.NodeId == "lonely");
    }

    [Fact]

    public async Task TheProblemListPutsErrorsFirstAndNamesTheNode()
    {
        (FlowValidationCoordinator coordinator, _, _) = Make(_ => new FlowValidationReport(
        [
            new FlowProblem("bpm.warn", "A warning.", FlowProblemSeverity.Warning, "approve"),
            new FlowProblem("bpm.error", "An error.", FlowProblemSeverity.Error, "start"),
        ], 2));

        coordinator.MarkEdited(TimeSpan.Zero);
        await coordinator.PollAsync(TimeSpan.FromMilliseconds(300), CancellationToken.None);

        IReadOnlyList<FlowProblemRow> rows = FlowProblemList.Rows(coordinator.Problems);
        Assert.Equal("bpm.error: An error.", rows[0].Text);
        Assert.Equal("bpm.warn: A warning.", rows[1].Text);
        Assert.True(rows[0].HasTarget);
        Assert.Contains("(start)", rows[0].AutomationName, StringComparison.Ordinal);
        Assert.Empty(FlowProblemList.Rows([]));
    }

    [Fact]

    public void ALoopIsReportedAsAHintWithTheNodeItPointsAt()
    {
        FlowDocument document = FlowFixtures.Document("document_then_notify.toml");
        document.AddEdge(new FlowEdge("notify", "start"));

        IReadOnlyList<FlowProblem> hints = LocalHints.Check(document);

        Assert.Contains(hints, problem => problem.Code == "hint.cycle" && problem.Severity == FlowProblemSeverity.Hint);
    }

    [Fact]

    public async Task ADeletedNodeLeavesNoProblemBehindBecauseTheServerIsAskedAgain()
    {
        int calls = 0;
        (FlowValidationCoordinator coordinator, FakeServer server, FlowEditor editor) = Make(_ =>
        {
            calls++;
            return new FlowValidationReport(
                calls == 1 ? [new FlowProblem("bpm.gone", "gone", FlowProblemSeverity.Error, "hold")] : [],
                calls);
        });

        coordinator.MarkEdited(TimeSpan.Zero);
        await coordinator.PollAsync(TimeSpan.FromMilliseconds(300), CancellationToken.None);
        Assert.Single(coordinator.ForNode("hold"));

        editor.Execute(new DeleteSelectionCommand(["hold"], []));
        coordinator.MarkEdited(TimeSpan.FromMilliseconds(400));
        await coordinator.PollAsync(TimeSpan.FromMilliseconds(800), CancellationToken.None);

        Assert.Equal(2, server.Validated.Count);
        Assert.Empty(coordinator.ForNode("hold"));
    }
}
