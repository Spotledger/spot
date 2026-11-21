using Spotledger.AdminCenter.Identity;
using Xunit;

namespace Spotledger.AdminCenter.Identity.Tests;

public sealed class AssignmentTests
{
    private static readonly DateOnly Today = new(2026, 10, 23);

    private static (AssignmentWizardViewModel Wizard, FakeGateway Gw) Wizard()
    {
        var gw = new FakeGateway();
        var wizard = new AssignmentWizardViewModel(gw, new FakeShell())
        {
            UserId = "ann",
            RoleId = "role-ap",
            CompanyId = "acme-pk",
            IncludeDescendants = true,
        };
        Assert.True(wizard.Next());
        return (wizard, gw);
    }

    [Fact]
    public void ChoosingNeedsAUserAndARole()
    {
        var wizard = new AssignmentWizardViewModel(new FakeGateway(), new FakeShell());
        Assert.False(wizard.Next());
        Assert.Equal(GrantStep.Choose, wizard.Step);
        Assert.NotNull(wizard.Error);
    }

    [Fact]
    public async Task TheCheckResultIsShownBeforeAnythingIsGranted()
    {
        var (wizard, gw) = Wizard();
        gw.Sod = new SodResult(SodOutcome.NeedsApproval, [new SodFinding("sod-1", "Creates and approves payments.", "payment-approver")]);
        Assert.True(await wizard.RunCheckAsync(Today));
        Assert.Equal(GrantStep.Check, wizard.Step);
        Assert.Equal(SodOutcome.NeedsApproval, wizard.Sod?.Outcome);
        Assert.Single(wizard.Sod!.Findings);
        Assert.DoesNotContain("Grant", gw.Calls);
        Assert.Equal("Request approval", wizard.ConfirmLabel);
    }

    [Fact]
    public async Task ABlockedGrantCannotBeConfirmed()
    {
        var (wizard, gw) = Wizard();
        gw.Sod = new SodResult(SodOutcome.Blocked, [new SodFinding("sod-9", "Forbidden pair.", "ledger-admin")]);
        await wizard.RunCheckAsync(Today);
        Assert.False(wizard.CanConfirm);
        Assert.False(await wizard.ConfirmAsync());
        Assert.Contains("blocked", wizard.Error, StringComparison.Ordinal);
        Assert.DoesNotContain("Grant", gw.Calls);
        Assert.Equal(GrantStep.Check, wizard.Step);
    }

    [Fact]
    public async Task AnOverrideNeedsAReasonAndAMitigatingControl()
    {
        var (wizard, gw) = Wizard();
        gw.Sod = new SodResult(SodOutcome.NeedsOverride, [new SodFinding("sod-2", "Conflict.", "buyer")]);
        await wizard.RunCheckAsync(Today);
        Assert.False(wizard.CanConfirm);
        wizard.OverrideReason = "Cover for leave";
        Assert.False(wizard.CanConfirm);
        wizard.MitigatingControl = "  ";
        Assert.False(wizard.CanConfirm);
        wizard.MitigatingControl = "Monthly review by the controller";
        Assert.True(wizard.CanConfirm);

        Assert.True(await wizard.ConfirmAsync());
        Assert.Equal("Cover for leave", gw.LastGrant?.Override?.Reason);
        Assert.Equal("Monthly review by the controller", gw.LastGrant?.Override?.MitigatingControl);
        Assert.Equal(new AssignmentScope("acme-pk", true), gw.LastGrant?.Request.Scope);
    }

    [Fact]
    public async Task AClearGrantSendsNoOverride()
    {
        var (wizard, gw) = Wizard();
        Assert.True(await wizard.RunCheckAsync(Today));
        Assert.True(await wizard.ConfirmAsync());
        Assert.Null(gw.LastGrant?.Override);
        Assert.Equal("Assignment granted.", wizard.Status);
    }

    [Fact]
    public async Task AGrantThatNeedsApprovalSaysNothingIsGrantedYet()
    {
        var (wizard, gw) = Wizard();
        gw.Sod = new SodResult(SodOutcome.NeedsApproval, [new SodFinding("sod-3", "Conflict.", "x")]);
        gw.Grant = GrantOutcome.ApprovalRequested;
        await wizard.RunCheckAsync(Today);
        Assert.True(await wizard.ConfirmAsync());
        Assert.Equal(GrantOutcome.ApprovalRequested, wizard.Outcome);
        Assert.Contains("Nothing is granted yet", wizard.Status, StringComparison.Ordinal);
    }

    [Fact]
    public async Task DatesAreCheckedAgainstTheDayPassedIn()
    {
        var (wizard, gw) = Wizard();
        wizard.ValidTo = Today.AddDays(-1);
        Assert.False(await wizard.RunCheckAsync(Today));
        wizard.ValidFrom = Today.AddDays(10);
        wizard.ValidTo = Today.AddDays(5);
        Assert.False(await wizard.RunCheckAsync(Today));
        Assert.DoesNotContain("CheckSod", gw.Calls);
        wizard.ValidTo = Today.AddDays(30);
        Assert.True(await wizard.RunCheckAsync(Today));
    }

    [Fact]
    public async Task GoingBackForgetsTheCheck()
    {
        var (wizard, _) = Wizard();
        await wizard.RunCheckAsync(Today);
        wizard.Back();
        Assert.Equal(GrantStep.ScopeAndDates, wizard.Step);
        Assert.Null(wizard.Sod);
        Assert.False(wizard.CanConfirm);
    }

    [Fact]
    public void AScopeWithoutACompanyIsEverywhereAndIgnoresDescendants()
    {
        var wizard = new AssignmentWizardViewModel(new FakeGateway(), new FakeShell()) { IncludeDescendants = true };
        Assert.Equal(new AssignmentScope(null, false), wizard.Scope);
        Assert.Equal("All companies", wizard.Scope.Describe());
    }

    private static AssignmentInfo Assign(string id, AssignmentSource source) =>
        new(id, "ann", "role-ap", new AssignmentScope("acme-pk", false), null, null, source);

    [Fact]
    public async Task AnIdpManagedAssignmentIsReadOnlyWithItsReason()
    {
        var gw = new FakeGateway();
        var idp = Assign("a-idp", AssignmentSource.Idp);
        var manual = Assign("a-man", AssignmentSource.Manual);
        gw.Assignments.AddRange([idp, manual, Assign("a-rule", AssignmentSource.Rule), Assign("a-em", AssignmentSource.Emergency)]);
        var vm = new AssignmentsViewModel(gw, new FakeShell());
        await vm.LoadAsync("ann");
        Assert.Equal(4, vm.Items.Count);
        Assert.True(idp.ManagedByIdp);
        Assert.False(AssignmentsViewModel.CanRevoke(idp));
        Assert.True(AssignmentsViewModel.CanRevoke(manual));

        Assert.False(await vm.RevokeAsync(idp));
        Assert.Contains("identity provider", vm.Error, StringComparison.Ordinal);
        Assert.DoesNotContain("Revoke", gw.Calls);

        Assert.False(await vm.RevokeAsync(Assign("a-em", AssignmentSource.Emergency)));
        Assert.True(await vm.RevokeAsync(manual));
        Assert.DoesNotContain(vm.Items, i => i.Id == "a-man");
    }

    [Fact]
    public async Task RevokingNeedsAConfirmation()
    {
        var gw = new FakeGateway();
        var manual = Assign("a-man", AssignmentSource.Manual);
        gw.Assignments.Add(manual);
        var shell = new FakeShell { ConfirmAnswer = false };
        var vm = new AssignmentsViewModel(gw, shell);
        await vm.LoadAsync("ann");
        Assert.False(await vm.RevokeAsync(manual));
        Assert.DoesNotContain("Revoke", gw.Calls);
    }

    [Fact]
    public async Task RolesSplitIntoReferenceAndCustomAndAReferenceIsCopiedNotEdited()
    {
        var gw = new FakeGateway();
        gw.Roles.AddRange([
            new RoleSummary("r1", "Accountant", RoleKind.Reference, null, "1.0", "1.1", ["post-journal"]),
            new RoleSummary("r2", "Our accountant", RoleKind.Custom, "r1", "1.0", null, ["post-journal"]),
        ]);
        gw.Duties.Add(new DutyInfo("post-journal", "Post journals", ["create journal"]));
        var vm = new RolesViewModel(gw, new FakeShell());
        await vm.RefreshAsync();
        Assert.Single(vm.ReferenceRoles);
        Assert.Single(vm.CustomRoles);
        Assert.Single(vm.Duties);

        Assert.False(await vm.CopyAsync("r2", "x"));
        Assert.False(await vm.CopyAsync("r1", " "));
        Assert.True(await vm.CopyAsync("r1", "Branch accountant"));
        Assert.Equal(2, vm.CustomRoles.Count);
    }

    [Fact]
    public async Task TheUpgradeDiffIsViewedThenConfirmedThenApplied()
    {
        var gw = new FakeGateway();
        gw.Roles.Add(new RoleSummary("r1", "Accountant", RoleKind.Reference, null, "1.0", "1.1", []));
        gw.Roles.Add(new RoleSummary("r3", "Clerk", RoleKind.Reference, null, "1.0", null, []));
        gw.Diff = new UpgradeDiff("r1", "1.0", "1.1", [new DutyChange("approve-payment", DutyChangeKind.Added, "approve payment"), new DutyChange("legacy", DutyChangeKind.Removed, null)]);
        var shell = new FakeShell();
        var vm = new RolesViewModel(gw, shell);
        await vm.RefreshAsync();

        Assert.False(await vm.ApplyUpgradeAsync());
        Assert.False(await vm.ViewUpgradeAsync("r3"));
        Assert.True(await vm.ViewUpgradeAsync("r1"));
        Assert.Equal(2, vm.Diff?.Changes.Count);
        shell.ConfirmAnswer = false;
        Assert.False(await vm.ApplyUpgradeAsync());
        shell.ConfirmAnswer = true;
        Assert.True(await vm.ApplyUpgradeAsync());
        Assert.Contains("1 removed", shell.Confirms[^1], StringComparison.Ordinal);
        Assert.Null(vm.Diff);
        Assert.Equal("1.1", vm.ReferenceRoles.First(r => r.Id == "r1").PackVersion);
    }

    [Fact]
    public void GrantingToYourselfIsRefused()
    {
        var wizard = new AssignmentWizardViewModel(new FakeGateway(), new FakeShell())
        {
            CurrentUserId = "ann",
            UserId = "ann",
            RoleId = "r1",
        };
        Assert.False(wizard.Next());
        Assert.Equal(GrantStep.Choose, wizard.Step);
        Assert.NotNull(wizard.Error);
    }
}
