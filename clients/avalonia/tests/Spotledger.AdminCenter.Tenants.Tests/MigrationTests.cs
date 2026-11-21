using Spotledger.AdminCenter.Tenants;
using Xunit;

namespace Spotledger.AdminCenter.Tenants.Tests;

public sealed class MigrationTests
{
    private const long Now = FakeTenantsApi.Now;

    private static async Task<(MigrationsBoard Board, FakeTenantsApi Api)> LoadedAsync()
    {
        var (api, context) = Fixtures.Make();
        var board = new MigrationsBoard(context, "t1");
        await board.LoadAsync(Now);
        await board.SelectAsync("p1");
        return (board, api);
    }

    private static async Task ApproveAsync(MigrationsBoard board)
    {
        board.BeginApprove();
        await board.Gate!.ApproveAsync(new FakeSigner("ann"));
        await board.Gate.ApproveAsync(new FakeSigner("bob"));
        Assert.True(await board.ApproveAsync(Now));
    }

    [Fact]
    public async Task TheBoardShowsThePlanWithRiskClassesAndThePolicy()
    {
        var (board, _) = await LoadedAsync();
        Assert.Equal(MigrationStage.Plan, board.Stage);
        Assert.Equal(MigrationRisk.NeedsReview, board.Plan!.Risk);
        Assert.Equal(2, board.Plan.Changes.Count);
        Assert.Equal("2 approvals needed. A review plan needs two approvers.", board.ApprovalText);
        Assert.Equal("ann", board.History[0].Actor);
        Assert.True(board.CanApprove);
        Assert.False(board.CanRehearse);
        Assert.False(board.CanApply);
        Assert.False(board.CanVerify);
    }

    [Fact]
    public async Task StagesCannotBeSkipped()
    {
        var (board, api) = await LoadedAsync();
        Assert.False(await board.RehearseAsync());
        Assert.False(await board.ApplyAsync());
        Assert.Contains("Approve the plan first", board.Error, StringComparison.Ordinal);
        Assert.False(await board.VerifyAsync());
        Assert.DoesNotContain("apply", api.Calls);
        Assert.DoesNotContain("rehearse", api.Calls);
    }

    [Fact]
    public async Task ApprovalWaitsForTheSignaturesThePolicyNeeds()
    {
        var (board, api) = await LoadedAsync();
        Assert.True(board.BeginApprove());
        await board.Gate!.ApproveAsync(new FakeSigner("ann"));
        Assert.False(await board.ApproveAsync(Now));
        Assert.Equal(MigrationStage.Plan, board.Stage);
        Assert.Empty(api.Executed);
        await board.Gate.ApproveAsync(new FakeSigner("bob"));
        Assert.True(await board.ApproveAsync(Now));
        Assert.Equal(MigrationStage.Approve, board.Stage);
        Assert.False(board.CanApprove);
        Assert.True(board.CanRehearse);
    }

    [Fact]
    public async Task ABlockedPlanCannotBeApproved()
    {
        var (api, context) = Fixtures.Make();
        api.MigrationPlan = api.MigrationPlan with { Conflicts = [new MigrationConflict("mig.clash", "Clash with app X.", true)] };
        var board = new MigrationsBoard(context, "t1");
        await board.LoadAsync(Now);
        await board.SelectAsync("p1");
        Assert.True(board.Plan!.Blocked);
        Assert.False(board.BeginApprove());
        Assert.Contains("blocking conflict", board.Error, StringComparison.Ordinal);
        Assert.Null(board.Gate);
    }

    [Fact]
    public async Task ARehearsalShowsTheFingerprintDiffAndAFailureKeepsTheBoardAtApprove()
    {
        var (board, api) = await LoadedAsync();
        await ApproveAsync(board);
        api.Rehearsal = new RehearsalReport(false, ["invoice fingerprint changed unexpectedly"], [new FingerprintDiff("invoice", "ab12", "ff00", false)]);
        Assert.False(await board.RehearseAsync());
        Assert.Equal(MigrationStage.Approve, board.Stage);
        Assert.False(board.Rehearsal!.Diffs[0].Expected);
        Assert.Equal("invoice fingerprint changed unexpectedly", board.Rehearsal.Problems[0]);
        Assert.False(board.CanApply);
        Assert.False(await board.ApplyAsync());

        api.Rehearsal = new RehearsalReport(true, [], [new FingerprintDiff("invoice", "ab12", "cd34", true)]);
        Assert.True(await board.RehearseAsync());
        Assert.Equal(MigrationStage.Rehearse, board.Stage);
    }

    [Fact]
    public async Task ADestructivePlanMustBeRehearsedBeforeItIsApplied()
    {
        var (api, context) = Fixtures.Make();
        api.MigrationPlan = api.MigrationPlan with { Risk = MigrationRisk.Destructive };
        var board = new MigrationsBoard(context, "t1");
        await board.LoadAsync(Now);
        await board.SelectAsync("p1");
        await ApproveAsync(board);
        Assert.True(board.RehearsalRequired);
        Assert.False(board.CanApply);
        Assert.False(await board.ApplyAsync());
        Assert.Contains("rehearsal", board.Error, StringComparison.Ordinal);
        await board.RehearseAsync();
        Assert.True(board.CanApply);
    }

    [Fact]
    public async Task APlanWithoutRehearsalRequirementMayBeAppliedRightAfterApproval()
    {
        var (board, _) = await LoadedAsync();
        await ApproveAsync(board);
        Assert.False(board.RehearsalRequired);
        Assert.True(board.CanApply);
    }

    [Fact]
    public async Task ApplyFollowsLiveProgressThenVerifyClosesTheBoard()
    {
        var (board, api) = await LoadedAsync();
        await ApproveAsync(board);
        await board.RehearseAsync();
        Assert.True(await board.ApplyAsync());
        Assert.Equal(["Add column", "Backfill", "Finalise"], board.Progress);
        Assert.Equal(10_000, board.PercentBasisPoints);
        Assert.Equal("Finalise", board.CurrentStep);
        Assert.Equal(MigrationStage.Apply, board.Stage);
        Assert.True(board.CanVerify);

        Assert.True(await board.VerifyAsync());
        Assert.Equal(MigrationStage.Verify, board.Stage);
        Assert.Equal(["fingerprints match", "app starts"], board.Verify!.Checks);
        Assert.Equal(["execute migration.approve", "rehearse", "apply", "verify"], api.Calls.Where(c => c is "rehearse" or "apply" or "verify" || c.StartsWith("execute", StringComparison.Ordinal)));
    }

    [Fact]
    public async Task AFailedApplyShowsRollbackEvidenceAndCannotBeVerified()
    {
        var (board, api) = await LoadedAsync();
        await ApproveAsync(board);
        api.ApplyScript =
        [
            new MigrationProgress("Add column", 2_500),
            new MigrationProgress("Backfill", 5_000, new ApplyOutcome(false, "Backfill timed out.", "snapshot s-17 restored; fingerprints equal the pre-apply ones")),
        ];
        Assert.False(await board.ApplyAsync());
        Assert.Equal("snapshot s-17 restored; fingerprints equal the pre-apply ones", board.Outcome!.RollbackEvidence);
        Assert.Contains("rolled back", board.Error, StringComparison.Ordinal);
        Assert.Equal(MigrationStage.Approve, board.Stage);
        Assert.False(board.CanVerify);
        Assert.False(await board.VerifyAsync());
        Assert.False(board.IsApplying);
    }

    [Fact]
    public async Task AFailedVerifyKeepsTheBoardAtApply()
    {
        var (board, api) = await LoadedAsync();
        await ApproveAsync(board);
        await board.ApplyAsync();
        api.VerifyResultValue = new VerifyResult(false, ["fingerprint mismatch"]);
        Assert.False(await board.VerifyAsync());
        Assert.Equal(MigrationStage.Apply, board.Stage);
        Assert.False(board.Verify!.Ok);
    }

    [Fact]
    public async Task ReloadingKeepsTheFurthestStage()
    {
        var (board, _) = await LoadedAsync();
        await ApproveAsync(board);
        await board.LoadAsync(Now);
        Assert.Equal(MigrationStage.Approve, board.Stage);
    }
}
