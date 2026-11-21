using Xunit;

namespace Spotledger.Client.Assistant.Tests;

public sealed class FlowTests
{
    private long _now = 1_000;

    private AssistantViewModel Make(FakeServer server) =>
        new(server, () => _now, d => d.StartsWith("spot://doc/", StringComparison.Ordinal));

    [Fact]
    public async Task T1CacheFlushCompletesWithOneClickAndVerifies()
    {
        var server = new FakeServer();
        server.Script.Add(new TextDelta("The cache is **stale**."));
        server.Script.Add(new ToolEvent(new ToolActivity("looked at job queue", [new EvidenceRef("job 17", "spot://doc/job/17")])));
        server.Script.Add(new FixProposed(FakeServer.CachePlan()));
        var vm = Make(server);
        vm.Input = "my items look old";
        await vm.SendAsync(CancellationToken.None);

        var fix = Assert.Single(vm.Transcript.OfType<FixEntry>()).Flow;
        Assert.True(fix.PlanShown);
        Assert.DoesNotContain(server.Calls, c => c.StartsWith("apply", StringComparison.Ordinal));
        Assert.True(fix.CanApply);

        await fix.ApplyAsync(CancellationToken.None);
        Assert.Equal(FixPhase.Applied, fix.Phase);
        Assert.Contains("apply:hash-1", server.Calls);
        Assert.Null(server.LastTyped);
        Assert.Equal("Verified: queue is moving.", fix.Message);
    }

    [Fact]
    public async Task T2NeedsTheTypedPhraseAndSendsIt()
    {
        var server = new FakeServer();
        server.Script.Add(new FixProposed(FakeServer.RetryPlan()));
        var vm = Make(server);
        vm.Input = "jobs are stuck";
        await vm.SendAsync(CancellationToken.None);
        var fix = vm.Transcript.OfType<FixEntry>().Single().Flow;

        Assert.Equal("repair 2", fix.RequiredPhrase);
        Assert.False(fix.CanApply);
        await Assert.ThrowsAsync<InvalidOperationException>(() => fix.ApplyAsync(CancellationToken.None));
        fix.TypedConfirmation = "repair 3";
        Assert.False(fix.CanApply);
        fix.TypedConfirmation = " repair 2 ";
        Assert.True(fix.CanApply);
        await fix.ApplyAsync(CancellationToken.None);
        Assert.Equal("repair 2", server.LastTyped);
        Assert.Equal(FixPhase.Applied, fix.Phase);
    }

    [Fact]
    public async Task T3NeverAppliesAndRoutesToApproval()
    {
        var server = new FakeServer();
        server.Script.Add(new FixProposed(FakeServer.StructuralPlan()));
        var vm = Make(server);
        vm.Input = "add a field";
        await vm.SendAsync(CancellationToken.None);
        var fix = vm.Transcript.OfType<FixEntry>().Single().Flow;

        Assert.True(fix.NeedsApproval);
        Assert.False(fix.CanApply);
        await Assert.ThrowsAsync<InvalidOperationException>(() => fix.ApplyAsync(CancellationToken.None));
        await fix.RequestApprovalAsync(CancellationToken.None);
        Assert.Equal(FixPhase.NeedsApproval, fix.Phase);
        Assert.Equal("task-77", fix.ApprovalTaskId);
        Assert.DoesNotContain(server.Calls, c => c.StartsWith("apply", StringComparison.Ordinal));
    }

    [Fact]
    public void NothingRunsBeforeThePlanIsShown()
    {
        var flow = new FixFlow(FakeServer.CachePlan(), new FakeServer(), () => _now);
        Assert.False(flow.CanApply);
        flow.MarkPlanShown();
        Assert.True(flow.CanApply);
    }

    [Fact]
    public async Task PlanPreviewMatchesTheDto()
    {
        var server = new FakeServer();
        var plan = FakeServer.RetryPlan();
        server.Script.Add(new FixProposed(plan));
        var vm = Make(server);
        vm.Input = "x";
        await vm.SendAsync(CancellationToken.None);
        var shown = vm.Transcript.OfType<FixEntry>().Single().Flow.Plan;
        Assert.Same(plan, shown);
        Assert.Equal(2, shown.Steps.Count);
        Assert.Equal(["job:17", "job:18"], shown.Affected);
    }

    [Fact]
    public async Task UndoWorksInsideTheWindowAndIsRefusedAfter()
    {
        var server = new FakeServer { UndoDeadline = 5_000 };
        var inside = new FixFlow(FakeServer.CachePlan(), server, () => _now);
        inside.MarkPlanShown();
        await inside.ApplyAsync(CancellationToken.None);
        Assert.True(inside.CanUndo);
        await inside.UndoAsync(CancellationToken.None);
        Assert.Equal(FixPhase.Undone, inside.Phase);

        var late = new FixFlow(FakeServer.CachePlan(), server, () => _now);
        late.MarkPlanShown();
        await late.ApplyAsync(CancellationToken.None);
        _now = 5_001;
        Assert.False(late.CanUndo);
        await late.UndoAsync(CancellationToken.None);
        Assert.Equal(FixPhase.UndoExpired, late.Phase);
        Assert.Single(server.Calls, c => c == "undo");
    }

    [Fact]
    public async Task BudgetStopShowsABannerAndBlocksSending()
    {
        var server = new FakeServer();
        server.Script.Add(new BudgetStopped("The monthly AI budget is used up."));
        var vm = Make(server);
        vm.Input = "help";
        await vm.SendAsync(CancellationToken.None);
        Assert.Equal("The monthly AI budget is used up.", vm.BudgetBanner);
        vm.Input = "again";
        Assert.False(vm.CanSend);
    }

    [Fact]
    public async Task RemovedChipsAreNotSent()
    {
        var server = new FakeServer();
        var vm = Make(server);
        vm.SetContext("Sales Invoice list", "SINV-1", "jobs.stuck", "stuck for 3h");
        Assert.Equal(3, vm.Chips.Count);
        vm.RemoveChip(vm.Chips.Single(c => c.Kind == "document"));
        vm.Input = "why";
        await vm.SendAsync(CancellationToken.None);
        Assert.Equal(["screen", "error"], server.LastRequest!.Chips.Select(c => c.Kind));
    }

    [Fact]
    public async Task OfflineModeExplainsTheLastErrorFromTheCatalogue()
    {
        var server = new FakeServer();
        var vm = Make(server);
        vm.SetOffline(true);
        vm.SetContext("Home", null, "jobs.stuck", null);
        vm.Input = "what is wrong";
        await vm.SendAsync(CancellationToken.None);
        Assert.DoesNotContain("ask", server.Calls);
        var notice = vm.Transcript.OfType<NoticeEntry>().Single();
        Assert.Contains("background job is stuck", notice.Text, StringComparison.Ordinal);
        Assert.Contains("offline", vm.PrivacyText, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task UnavailableEventSwitchesToOffline()
    {
        var server = new FakeServer();
        server.Script.Add(new AiUnavailable("The AI service is down."));
        var vm = Make(server);
        vm.SetContext("Home", null, "api.unavailable", null);
        vm.Input = "hello";
        await vm.SendAsync(CancellationToken.None);
        Assert.True(vm.IsOffline);
    }

    [Fact]
    public async Task PrivacyIndicatorReportsTheEndpoint()
    {
        var server = new FakeServer { Endpoint = new("https://api.example", "big-model", true) };
        var vm = Make(server);
        await vm.RefreshEndpointAsync(CancellationToken.None);
        Assert.Contains("big-model", vm.PrivacyText, StringComparison.Ordinal);
        Assert.Contains("data leaves your tenant", vm.PrivacyText, StringComparison.Ordinal);
    }

    [Fact]
    public async Task HumanRequestSendsOnlyTheReviewedRedactedBundle()
    {
        var server = new FakeServer();
        var vm = Make(server);
        vm.SetContext("Home", null, "api.forbidden", "denied for ann@example.com token=abc123");
        vm.PrepareHumanRequest();
        Assert.NotNull(vm.PendingBundle);
        Assert.DoesNotContain("ann@example.com", vm.PendingBundle!.Text, StringComparison.Ordinal);
        Assert.DoesNotContain("abc123", vm.PendingBundle.Text, StringComparison.Ordinal);
        await Assert.ThrowsAsync<InvalidOperationException>(() => Make(server).SendHumanRequestAsync("help", CancellationToken.None));
        vm.EditBundle(vm.PendingBundle.Text + "\nextra password=hunter2");
        Assert.DoesNotContain("hunter2", vm.PendingBundle!.Text, StringComparison.Ordinal);
        await vm.SendHumanRequestAsync("please help", CancellationToken.None);
        Assert.Single(server.Calls, c => c.StartsWith("support:", StringComparison.Ordinal));
        Assert.Null(vm.PendingBundle);
    }

    [Fact]
    public void PanelWidthIsClampedAndToggleFlipsTheDock()
    {
        var vm = Make(new FakeServer());
        vm.PanelWidth = 10;
        Assert.Equal(AssistantViewModel.MinPanelWidth, vm.PanelWidth);
        vm.PanelWidth = 5000;
        Assert.Equal(AssistantViewModel.MaxPanelWidth, vm.PanelWidth);
        vm.ToggleCommand.Execute(null);
        Assert.True(vm.IsOpen);
    }

    [Fact]
    public void ToggleIsInTheCommandModel()
    {
        var registry = new Spotledger.Client.Shell.CommandRegistry();
        AssistantCommands.Register(registry);
        Assert.Equal("Ask the assistant", registry[AssistantCommands.Toggle].Label);
    }

    [Fact]
    public async Task CancelStopsTheStream()
    {
        var server = new FakeServer();
        for (var i = 0; i < 50; i++) { server.Script.Add(new TextDelta("x")); }
        var vm = Make(server);
        vm.Input = "go";
        using var cts = new CancellationTokenSource();
        cts.Cancel();
        await vm.SendAsync(cts.Token);
        Assert.False(vm.IsBusy);
        Assert.Contains(vm.Transcript, e => e is NoticeEntry);
    }

    [Fact]
    public async Task T2WithoutAffectedRecordsCannotBeApplied()
    {
        var server = new FakeServer();
        server.Script.Add(new FixProposed(FakeServer.RetryPlan() with { Affected = [] }));
        var vm = Make(server);
        vm.Input = "x";
        await vm.SendAsync(CancellationToken.None);
        var fix = vm.Transcript.OfType<FixEntry>().Single().Flow;
        fix.TypedConfirmation = "repair 0";
        Assert.False(fix.CanApply);
    }

    [Fact]
    public async Task ApplyFailureShowsNoRawExceptionText()
    {
        var server = new FakeServer { ApplyThrows = true };
        server.Script.Add(new FixProposed(FakeServer.CachePlan()));
        var vm = Make(server);
        vm.Input = "x";
        await vm.SendAsync(CancellationToken.None);
        var fix = vm.Transcript.OfType<FixEntry>().Single().Flow;
        await fix.ApplyAsync(CancellationToken.None);
        Assert.Equal(FixPhase.Failed, fix.Phase);
        Assert.DoesNotContain("SECRET", fix.Message, StringComparison.Ordinal);
    }

    [Fact]
    public async Task InputAndReplyAreCapped()
    {
        var server = new FakeServer();
        server.Script.Add(new TextDelta(new string('a', AssistantViewModel.MaxReplyLength + 5000)));
        var vm = Make(server);
        vm.Input = new string('q', AssistantViewModel.MaxInputLength + 100);
        await vm.SendAsync(CancellationToken.None);
        Assert.Equal(AssistantViewModel.MaxInputLength, server.LastRequest!.Text.Length);
        Assert.Equal(AssistantViewModel.MaxReplyLength, vm.Transcript.OfType<AssistantEntry>().Single().Text.Length);
    }
}
