using Spotledger.AdminCenter.Deploy;
using Xunit;

namespace Spotledger.AdminCenter.Deploy.Tests;

public sealed class ClusterWizardTests
{
    private static readonly string[] Hosts = ["app1.example.com", "db1.example.com", "db2.example.com"];

    private static (ClusterWizardViewModel Wizard, FakeEngine Engine, MemoryStore Store) Make()
    {
        var engine = new FakeEngine();
        engine.Servers.AddRange(Hosts.Select(FakeEngine.Facts));
        var store = new MemoryStore();
        return (new ClusterWizardViewModel(engine, store), engine, store);
    }

    private static async Task BuildThreeNodeAsync(ClusterWizardViewModel wizard)
    {
        await wizard.LoadServersAsync();
        foreach (var host in Hosts)
        {
            await wizard.AddServerAsync(host);
        }

        await wizard.AssignRoleAsync(NodeRole.Web, Hosts[0]);
        await wizard.AssignRoleAsync(NodeRole.Jobs, Hosts[0]);
        await wizard.AssignRoleAsync(NodeRole.Database, Hosts[1]);
        await wizard.AssignRoleAsync(NodeRole.Database, Hosts[2]);
        await wizard.SetWebAddressAsync("erp.example.com");
    }

    private static async Task ToSignStepAsync(ClusterWizardViewModel wizard)
    {
        await BuildThreeNodeAsync(wizard);
        Assert.True(await wizard.NextAsync());
        Assert.True(await wizard.NextAsync());
        Assert.True(await wizard.NextAsync());
        Assert.Equal(ClusterStep.Sign, wizard.Step);
    }

    [Fact]
    public async Task ThreeNodeHappyPathReachesAHealthyVerification()
    {
        var (wizard, _, store) = Make();
        await ToSignStepAsync(wizard);
        Assert.False(wizard.CanGoNext);
        Assert.False(await wizard.NextAsync());
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        Assert.True(await wizard.NextAsync());
        Assert.Equal(ClusterStep.Run, wizard.Step);

        Assert.True(await wizard.StartAsync());
        Assert.Equal(ClusterStep.Verify, wizard.Step);
        Assert.Equal(10_000, wizard.Run!.ProgressBasisPoints);
        Assert.All(wizard.Run.Rows, row => Assert.Equal(StepStatus.Done, row.Status));
        Assert.True(wizard.Verification!.Health.All(row => row.Healthy));
        Assert.Equal("https://erp.example.com", Assert.Single(wizard.Verification.Endpoints));
        Assert.Null(store.State);
    }

    [Fact]
    public async Task LiveValidationShowsEveryProblemWithItsFix()
    {
        var (wizard, _, _) = Make();
        await wizard.LoadServersAsync();
        await wizard.AddServerAsync(Hosts[0]);
        Assert.False(wizard.IsValid);
        Assert.Equal(2, wizard.Violations.Count);
        Assert.All(wizard.Violations, v => Assert.False(string.IsNullOrWhiteSpace(v.Fix)));

        await wizard.AssignRoleAsync(NodeRole.Database, Hosts[0]);
        Assert.Equal("web-required", Assert.Single(wizard.Violations).Rule);
        await wizard.AssignRoleAsync(NodeRole.Web, Hosts[0]);
        Assert.Equal("web-address", Assert.Single(wizard.Violations).Rule);
        await wizard.SetWebAddressAsync("erp.example.com");
        Assert.True(wizard.IsValid);
        Assert.Empty(wizard.Violations);
    }

    [Fact]
    public async Task AnUnknownServerCannotJoin()
    {
        var (wizard, _, _) = Make();
        await wizard.LoadServersAsync();
        Assert.False(await wizard.AddServerAsync("ghost.example.com"));
        Assert.Contains("Add server", wizard.Error, StringComparison.Ordinal);
        Assert.Empty(wizard.Nodes);
    }

    [Fact]
    public async Task ARoleMovesBetweenServersAndTheKeyboardPathDoesTheSame()
    {
        var (wizard, _, _) = Make();
        await BuildThreeNodeAsync(wizard);
        Assert.True(await wizard.MoveRoleAsync(NodeRole.Jobs, Hosts[0], Hosts[1]));
        Assert.DoesNotContain(NodeRole.Jobs, wizard.Nodes[0].Roles);
        Assert.Contains(NodeRole.Jobs, wizard.Nodes[1].Roles);

        Assert.False(await wizard.AssignSelectedAsync());
        wizard.SelectedRole = NodeRole.Realtime;
        wizard.SelectedHost = Hosts[2];
        Assert.True(await wizard.AssignSelectedAsync());
        Assert.Contains(NodeRole.Realtime, wizard.Nodes[2].Roles);
        Assert.True(await wizard.UnassignRoleAsync(NodeRole.Realtime, Hosts[2]));
        Assert.DoesNotContain(NodeRole.Realtime, wizard.Nodes[2].Roles);
    }

    [Fact]
    public async Task ABranchEdgeSharingAServerIsReportedLive()
    {
        var (wizard, _, _) = Make();
        await BuildThreeNodeAsync(wizard);
        await wizard.AssignRoleAsync(NodeRole.BranchEdge, Hosts[0]);
        Assert.Contains(wizard.Violations, v => v.Rule == "branch-edge-alone");
        Assert.False(await wizard.NextAsync() && wizard.Step == ClusterStep.Plan);
    }

    [Fact]
    public void ProfilesAndRolesCarryPlainExplanations()
    {
        Assert.Equal(3, ClusterWizardViewModel.Profiles.Count);
        Assert.All(ClusterWizardViewModel.Profiles, p => Assert.True(p.Explanation.Length > 40));
        Assert.Equal(5, ClusterWizardViewModel.Roles.Count);
        Assert.All(ClusterWizardViewModel.Roles, r => Assert.DoesNotContain("-", r.Label, StringComparison.Ordinal));
    }

    [Fact]
    public async Task AFailingPreflightStopsBeforeAPlanExists()
    {
        var (wizard, engine, _) = Make();
        await BuildThreeNodeAsync(wizard);
        engine.PreflightFor[Hosts[1]] = Severity.Fail;
        await wizard.NextAsync();
        Assert.False(await wizard.NextAsync());
        Assert.Equal(ClusterStep.Profile, wizard.Step);
        Assert.Equal(Severity.Fail, wizard.Preflight[0].Severity);
        Assert.Contains("block", wizard.Error, StringComparison.Ordinal);
        Assert.DoesNotContain("Plan", engine.Calls);

        engine.PreflightFor[Hosts[1]] = Severity.Warn;
        Assert.True(await wizard.NextAsync());
        Assert.Equal(ClusterStep.Plan, wizard.Step);
    }

    [Fact]
    public async Task TheEnginesRefusalToPlanIsShownInItsOwnWords()
    {
        var (wizard, engine, _) = Make();
        await BuildThreeNodeAsync(wizard);
        engine.Refuse = "A catalogue operation is missing.";
        await wizard.NextAsync();
        Assert.False(await wizard.NextAsync());
        Assert.Equal("A catalogue operation is missing.", wizard.Error);
        Assert.Equal(ClusterStep.Profile, wizard.Step);
    }

    [Fact]
    public async Task ThePlanPreviewShowsTheEstimateAndParallelism()
    {
        var (wizard, _, _) = Make();
        await BuildThreeNodeAsync(wizard);
        await wizard.NextAsync();
        await wizard.NextAsync();
        var plan = wizard.Plan!;
        Assert.Equal(900UL, plan.EstimatedTotalSeconds);
        Assert.True(plan.EstimatedTotalSeconds < plan.SerialSeconds);
        Assert.Contains(plan.Steps, step => step.NeedsCleanupBeforeRetry);
        wizard.Back();
        Assert.Null(wizard.Plan);
        Assert.Equal(ClusterStep.Profile, wizard.Step);
    }

    [Fact]
    public async Task SigningNeedsDistinctApprovers()
    {
        var (wizard, _, _) = Make();
        await ToSignStepAsync(wizard);
        var signing = wizard.Signing!;
        Assert.Equal(2, signing.Required);
        Assert.True(await signing.ApproveAsync(new FakeSigner("ann")));
        Assert.False(await signing.ApproveAsync(new FakeSigner("ann")));
        Assert.Contains("different person", signing.Error, StringComparison.Ordinal);
        Assert.Contains("another approver", signing.StatusText, StringComparison.Ordinal);
        Assert.False(signing.IsComplete);
        Assert.True(await signing.ApproveAsync(new FakeSigner("bob")));
        Assert.Equal(["ann", "bob"], signing.Signers);
        Assert.False(await signing.ApproveAsync(new FakeSigner("cy")));
    }

    [Fact]
    public async Task AFailedStepStopsTheRunAndRetryResumesAtItWithoutRepeatingDoneSteps()
    {
        var (wizard, engine, _) = Make();
        await ToSignStepAsync(wizard);
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        await wizard.NextAsync();
        engine.FailStep = 4;

        Assert.False(await wizard.StartAsync());
        var run = wizard.Run!;
        Assert.Equal(RunState.Failed, run.State);
        Assert.Equal(ClusterStep.Run, wizard.Step);
        Assert.Equal(4, run.FailedRow!.Index);
        Assert.Equal("The server stopped answering.", run.FailedRow.FailureReason);
        Assert.Contains("connection reset", run.SelectedLog);
        Assert.Equal(4, run.DoneSteps);
        Assert.True(run.CanRetry);
        Assert.Contains("again", run.RetryNote, StringComparison.Ordinal);
        Assert.Equal(["Retry", "Log"], wizard.FocusOrder);

        Assert.True(await wizard.RetryAsync());
        Assert.Equal(ClusterStep.Verify, wizard.Step);
        Assert.All(run.Rows, row => Assert.Equal(StepStatus.Done, row.Status));
        Assert.DoesNotContain(run.Rows, row => row.Log.Count(l => l.EndsWith("started", StringComparison.Ordinal)) > 1);
    }

    [Fact]
    public async Task ARetryNoteSaysWhenTheFailedStepIsCleanedUpFirst()
    {
        var (wizard, engine, _) = Make();
        await ToSignStepAsync(wizard);
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        await wizard.NextAsync();
        var databaseStep = wizard.Run!.Plan.Steps.First(step => step.NeedsCleanupBeforeRetry).Index;
        engine.FailStep = databaseStep;
        await wizard.StartAsync();
        Assert.Contains("cleans it up first", wizard.Run.RetryNote, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ClosingTheAppMidRunResumesFromTheJournal()
    {
        var (wizard, engine, store) = Make();
        await ToSignStepAsync(wizard);
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        await wizard.NextAsync();
        engine.FailStep = 5;
        await wizard.StartAsync();
        Assert.Equal(ClusterStep.Run, store.State!.Step);

        // A new app instance over the same engine and store.
        var reopened = new ClusterWizardViewModel(engine, store);
        Assert.True(await reopened.RestoreAsync());
        Assert.Equal(ClusterStep.Run, reopened.Step);
        Assert.Equal(3, reopened.Nodes.Count);
        Assert.Equal("erp.example.com", reopened.WebAddress);
        Assert.Equal(RunState.Failed, reopened.Run!.State);
        Assert.Equal(5, reopened.Run.FailedRow!.Index);
        Assert.Equal(5, reopened.Run.DoneSteps);

        Assert.True(await reopened.RetryAsync());
        Assert.Equal(ClusterStep.Verify, reopened.Step);
        Assert.Empty(engine.RunSignatures[^1]);
        Assert.Null(store.State);
    }

    [Fact]
    public async Task AJournalLeftPartWayCanBeContinued()
    {
        var (wizard, engine, store) = Make();
        await ToSignStepAsync(wizard);
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        await wizard.NextAsync();
        engine.StopAfterStep = 3; // the laptop is closed here: no failure, the journal just stops
        await wizard.StartAsync();

        var reopened = new ClusterWizardViewModel(engine, store);
        Assert.True(await reopened.RestoreAsync());
        var run = reopened.Run!;
        Assert.Equal(RunState.Interrupted, run.State);
        Assert.True(run.CanStart);
        Assert.Equal(4, run.DoneSteps);
        Assert.Contains("part-way", reopened.Status, StringComparison.Ordinal);

        Assert.True(await reopened.StartAsync());
        Assert.Equal(ClusterStep.Verify, reopened.Step);
        Assert.Equal(1, engine.Calls.Count(c => c == "Verify"));
    }

    [Fact]
    public async Task ARestoredCompletedDeploymentGoesStraightToVerification()
    {
        var (wizard, engine, store) = Make();
        await ToSignStepAsync(wizard);
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        await wizard.NextAsync();
        await wizard.StartAsync();
        // The app died before the saved place was cleared.
        store.State = new WizardState(new TopologyDraft([.. wizard.Nodes], wizard.Profile, wizard.WebAddress), wizard.Plan!.PlanId, ClusterStep.Run);

        var reopened = new ClusterWizardViewModel(engine, store);
        Assert.True(await reopened.RestoreAsync());
        Assert.Equal(ClusterStep.Verify, reopened.Step);
    }

    [Fact]
    public async Task NothingSavedMeansNothingToRestore()
    {
        var (wizard, _, _) = Make();
        Assert.False(await wizard.RestoreAsync());
        Assert.Equal(ClusterStep.Servers, wizard.Step);
    }

    [Fact]
    public async Task APlanOnlyPreviewedIsMadeAgainOnRestore()
    {
        var (wizard, engine, store) = Make();
        await BuildThreeNodeAsync(wizard);
        await wizard.NextAsync();
        await wizard.NextAsync();
        Assert.Equal(ClusterStep.Plan, store.State!.Step);

        var reopened = new ClusterWizardViewModel(engine, store);
        Assert.True(await reopened.RestoreAsync());
        Assert.Equal(ClusterStep.Profile, reopened.Step);
        Assert.Null(reopened.Plan);
    }

    [Fact]
    public async Task KeyboardOnlyCompletionOfTheWholeWizard()
    {
        var (wizard, _, _) = Make();
        Assert.Equal(["Servers", "Roles", "AssignSelected", "WebAddress", "Next"], wizard.FocusOrder);
        await wizard.LoadServersAsync();
        foreach (var host in Hosts)
        {
            await wizard.AddServerAsync(host);
        }

        foreach (var (role, host) in new[]
        {
            (NodeRole.Web, Hosts[0]), (NodeRole.Database, Hosts[1]), (NodeRole.Database, Hosts[2]),
        })
        {
            wizard.SelectedRole = role;
            wizard.SelectedHost = host;
            Assert.True(await wizard.AssignSelectedAsync());
        }

        await wizard.SetWebAddressAsync("erp.example.com");
        Assert.True(await wizard.SetProfileAsync(DeployProfile.ClusterBasic));
        Assert.True(await wizard.NextAsync());
        Assert.Equal(["Profile", "Back", "Next"], wizard.FocusOrder);
        Assert.True(await wizard.NextAsync());
        Assert.True(await wizard.NextAsync());
        await wizard.Signing!.ApproveAsync(new FakeSigner("ann"));
        await wizard.Signing.ApproveAsync(new FakeSigner("bob"));
        Assert.True(await wizard.NextAsync());
        Assert.Equal(["Start", "Log"], wizard.FocusOrder);
        Assert.True(await wizard.StartAsync());
        Assert.Equal(["Close"], wizard.FocusOrder);
    }
}
