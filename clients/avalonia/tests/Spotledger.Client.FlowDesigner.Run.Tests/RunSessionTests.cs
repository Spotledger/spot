using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The test-run session: it sends the process as its canonical text form with the sample data, keeps
/// the server's snapshot as the truth, and turns every answer into a badge, a status line or the
/// history it opens when the run ends.
public sealed class RunSessionTests
{

    [Fact]

    public async Task AStartSendsTheTextFormAndTheSampleData()
    {
        FakeRunServer server = new();
        FlowDocument document = RunFixtures.ScenarioDocument();
        FlowRunSession session = new(server, document, RunFixtures.Catalogue());
        session.SampleData.Text = RunFixtures.SampleJson;

        FlowActionResult result = await session.StartTestAsync(TestContext.Current.CancellationToken);

        Assert.True(result.Ok);
        DryRunRequest request = Assert.Single(server.Started);
        Assert.Equal("order_intake", request.ProcessName);
        Assert.Equal(3, request.Version);
        Assert.Equal(document.ToText(), request.TextForm, StringComparer.Ordinal);
        Assert.Equal(RunFixtures.SampleJson, request.SampleDataJson, StringComparer.Ordinal);
        Assert.Equal("Test request run_1 started.", result.Message);
        Assert.Contains("run_1", session.StatusMessage!, StringComparison.Ordinal);
        Assert.True(session.HasRun);
        Assert.True(session.IsRunning);
    }

    [Fact]

    public async Task ASampleThatDoesNotParseNeverReachesTheServer()
    {
        FakeRunServer server = new();
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.SampleData.Text = "{\"path\": }";

        FlowActionResult result = await session.StartTestAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.Empty(server.Started);
        Assert.NotNull(session.ErrorMessage);
        Assert.Contains("line 1", session.ErrorMessage!, StringComparison.Ordinal);
        Assert.Null(session.Snapshot);
    }

    [Fact]

    public async Task TheBadgesFollowEverySnapshot()
    {
        FakeRunServer server = new();
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.SampleData.Text = RunFixtures.SampleJson;
        await session.StartTestAsync(TestContext.Current.CancellationToken);

        Assert.Equal(4, session.Badges.Count);
        Assert.Equal(RunNodeStatus.Running, session.Badge("webhook")!.Status);
        Assert.Equal("Running", session.Badge("webhook")!.Text);
        Assert.Equal(RunNodeStatus.Pending, session.Badge("mail")!.Status);
        Assert.Equal(
            "Node webhook: Running, attempt 1",
            session.Badge("webhook")!.HelpText);

        server.Enqueue(RunFixtures.Snapshot(
            "run_1",
            RunOutcome.Running,
            ("webhook", RunNodeStatus.Succeeded),
            ("document", RunNodeStatus.Running),
            ("http", RunNodeStatus.Pending),
            ("mail", RunNodeStatus.Pending)));
        FlowActionResult step = await session.StepAsync(TestContext.Current.CancellationToken);

        Assert.True(step.Ok);
        Assert.Equal(("run_1", RunStepMode.Step), server.Steps[0]);
        Assert.Equal("Stepped to node webhook.", step.Message);
        Assert.Equal(RunNodeStatus.Succeeded, session.Badge("webhook")!.Status);
        Assert.Equal(RunNodeStatus.Running, session.Badge("document")!.Status);
        Assert.Null(session.ErrorMessage);
    }

    [Fact]

    public async Task TheStepAndContinueFlagsAreTheServers()
    {
        FakeRunServer server = new()
        {
            StartResult = new RunSnapshot(
                "run_2",
                RunOutcome.Running,
                [new RunNodeSnapshot("webhook", RunNodeStatus.Waiting)],
                CanStep: false,
                CanContinue: true),
        };
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.SampleData.Text = RunFixtures.SampleJson;

        await session.StartTestAsync(TestContext.Current.CancellationToken);

        Assert.False(session.CanStep);
        Assert.True(session.CanContinue);
        Assert.True(session.CanStop);

        server.Enqueue(RunFixtures.Snapshot("run_2", RunOutcome.Succeeded, ("webhook", RunNodeStatus.Succeeded)));
        server.Detail = RunFixtures.ScenarioDetail();

        FlowActionResult continued = await session.ContinueAsync(TestContext.Current.CancellationToken);

        Assert.True(continued.Ok);
        Assert.Equal(RunStepMode.Continue, server.Steps[0].Mode);
        Assert.False(session.CanStep);
        Assert.False(session.CanContinue);
        Assert.False(session.CanStop);
        Assert.False(session.IsRunning);
        Assert.Equal("Test request finished: succeeded.", continued.Message);
        Assert.NotNull(session.Detail);
        Assert.Equal(5, session.History.Timeline.Count);
    }

    [Fact]

    public async Task ARefusedStepShowsTheServersReason()
    {
        FakeRunServer server = new();
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.SampleData.Text = RunFixtures.SampleJson;
        await session.StartTestAsync(TestContext.Current.CancellationToken);
        server.StepFault = new RunRefusedException("bpm.run.step.refused", "the graph is still running a step.");

        FlowActionResult result = await session.StepAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.Equal("the graph is still running a step.", session.ErrorMessage);
        Assert.Null(session.StatusMessage);
    }

    [Fact]

    public async Task ARunThatHasNotStartedCannotBeStepped()
    {
        FakeRunServer server = new();
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());

        FlowActionResult result = await session.StepAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.Equal(RunStrings.NoRunYet, session.ErrorMessage);
        Assert.Empty(server.Steps);
        Assert.Empty(session.Badges);
    }

    [Fact]

    public async Task AStopLoadsTheHistoryAsItStands()
    {
        FakeRunServer server = new() { Detail = RunFixtures.ScenarioDetail() };
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.SampleData.Text = RunFixtures.SampleJson;
        await session.StartTestAsync(TestContext.Current.CancellationToken);
        server.StopResult = RunFixtures.Snapshot(
            "run_1",
            RunOutcome.Stopped,
            ("webhook", RunNodeStatus.Stopped),
            ("document", RunNodeStatus.Skipped));

        FlowActionResult result = await session.StopAsync(TestContext.Current.CancellationToken);

        Assert.True(result.Ok);
        Assert.Equal(["run_1"], server.Stops);
        Assert.Equal("Test request stopped.", result.Message);
        Assert.NotNull(session.Detail);
        Assert.Equal(5, session.History.Timeline.Count);
        Assert.Equal(RunNodeStatus.Stopped, session.Badge("webhook")!.Status);
    }

    [Fact]

    public async Task TheRunListIsQueriedForThisProcessAndVersion()
    {
        FakeRunServer server = new() { Runs = RunFixtures.Runs() };
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());

        IReadOnlyList<RunSummary> runs = await session.LoadRunsAsync(TestContext.Current.CancellationToken);

        RunQuery query = Assert.Single(server.Queries);
        Assert.Equal("order_intake", query.ProcessName);
        Assert.Equal(3, query.Version);
        Assert.Equal(3, session.History.Groups.Count);
        Assert.Equal(4, runs.Count);

        server.Detail = RunFixtures.ScenarioDetail();
        RunDetail? detail = await session.OpenRunAsync("run_7", TestContext.Current.CancellationToken);

        Assert.NotNull(detail);
        Assert.Equal("run_7", session.History.Selected!.RunId);
    }

    [Fact]

    public void ACredentialLearnedLaterScrubsTheNextMessage()
    {
        FakeRunServer server = new();
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.AddSecret(new RunSecret("api", RunFixtures.Canary));

        session.ReportError("the peer echoed " + RunFixtures.Canary);

        Assert.NotNull(session.ErrorMessage);
        Assert.DoesNotContain(RunFixtures.Canary, session.ErrorMessage!, StringComparison.Ordinal);
        Assert.Contains("[redacted:api]", session.ErrorMessage!, StringComparison.Ordinal);
        Assert.Single(session.Secrets);
    }

    [Fact]

    public async Task AChangeIsReportedToAHostAfterEveryAnswer()
    {
        FakeRunServer server = new();
        FlowRunSession session = new(server, RunFixtures.ScenarioDocument(), RunFixtures.Catalogue());
        session.SampleData.Text = RunFixtures.SampleJson;
        int changes = 0;
        session.Changed += (_, _) => changes++;

        await session.StartTestAsync(TestContext.Current.CancellationToken);
        server.Enqueue(RunFixtures.Snapshot("run_1", RunOutcome.Succeeded, ("webhook", RunNodeStatus.Succeeded)));
        server.Detail = RunFixtures.ScenarioDetail();

        Assert.True(changes >= 2);

        await session.ContinueAsync(TestContext.Current.CancellationToken);

        Assert.False(session.IsRunning);
        Assert.False(session.CanContinue);
    }
}
