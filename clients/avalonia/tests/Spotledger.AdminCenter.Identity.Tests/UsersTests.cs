using Spotledger.AdminCenter.Identity;
using Xunit;

namespace Spotledger.AdminCenter.Identity.Tests;

public sealed class UsersTests
{
    private static UserSummary Person(string id, string name, UserState state = UserState.Active) =>
        new(id, name, $"{id}@acme.test", UserKind.Person, state, null, ["Accountant"]);

    private static (UsersViewModel Vm, FakeGateway Gw, FakeShell Shell) Make()
    {
        var gw = new FakeGateway();
        var shell = new FakeShell();
        gw.Users.AddRange([Person("ann", "Ann Lee"), Person("bob", "Bob Ray", UserState.Disabled), new("svc", "Importer", "ops@acme.test", UserKind.Service, UserState.Active, null, [])]);
        return (new UsersViewModel(gw, shell), gw, shell);
    }

    [Fact]
    public async Task ListSearchAndFilterGoToTheServer()
    {
        var (vm, gw, _) = Make();
        await vm.RefreshAsync();
        Assert.Equal(3, vm.Users.Count);
        vm.Text = "ann";
        await vm.RefreshAsync();
        Assert.Equal(["ann"], vm.Users.Select(u => u.Id));
        vm.Text = null;
        vm.KindFilter = UserKind.Service;
        await vm.RefreshAsync();
        Assert.Equal(["svc"], vm.Users.Select(u => u.Id));
        vm.KindFilter = null;
        vm.StateFilter = UserState.Disabled;
        await vm.RefreshAsync();
        Assert.Equal(["bob"], vm.Users.Select(u => u.Id));
        Assert.Equal(UserState.Disabled, gw.LastFilter?.State);
    }

    [Fact]
    public async Task OnboardingFlowInvitesShowsTheLinkOnceAndForgetsIt()
    {
        var (vm, gw, _) = Make();
        var ok = await vm.InviteAsync(new InviteRequest("new@acme.test", "New Person", ["role-accountant"]));
        Assert.True(ok);
        Assert.NotNull(vm.RevealedSecret);
        Assert.Contains("SECRET-LINK", vm.RevealedSecret!.Value, StringComparison.Ordinal);
        Assert.DoesNotContain("SECRET-LINK", vm.RevealedSecret.ToString(), StringComparison.Ordinal);
        Assert.DoesNotContain("SECRET-LINK", vm.Status, StringComparison.Ordinal);
        vm.DismissSecret();
        Assert.Null(vm.RevealedSecret);
        Assert.Contains("Invite", gw.Calls);
    }

    [Fact]
    public async Task AnInvitationNeedsAnEmailAndAName()
    {
        var (vm, gw, _) = Make();
        Assert.False(await vm.InviteAsync(new InviteRequest("nobody", "X", [])));
        Assert.False(await vm.InviteAsync(new InviteRequest("a@b.test", " ", [])));
        Assert.DoesNotContain("Invite", gw.Calls);
    }

    [Fact]
    public async Task AServiceUserShowsItsKeyOnce()
    {
        var (vm, _, _) = Make();
        Assert.True(await vm.CreateServiceUserAsync("Nightly import", "ann"));
        Assert.Equal("API key", vm.RevealedSecret?.Label);
        Assert.DoesNotContain("KEY_SECRET", vm.Status, StringComparison.Ordinal);
        Assert.False(await vm.CreateServiceUserAsync("", "ann"));
    }

    [Fact]
    public async Task DisablingNeedsThePreviewFirstThenAConfirmation()
    {
        var (vm, gw, shell) = Make();
        gw.Leaver = new LeaverPreview("ann", 2, 1, 1, [new TaskReassignment("Approve payroll", "bob")]);
        Assert.False(await vm.DisableAsync("ann"));
        Assert.DoesNotContain("Disable", gw.Calls);

        Assert.True(await vm.PreviewDisableAsync("ann"));
        Assert.Equal(2, vm.LeaverPreview?.Sessions);
        shell.ConfirmAnswer = false;
        Assert.False(await vm.DisableAsync("ann"));
        Assert.DoesNotContain("Disable", gw.Calls);

        shell.ConfirmAnswer = true;
        Assert.True(await vm.DisableAsync("ann"));
        Assert.Contains("2 sessions end", shell.Confirms[^1], StringComparison.Ordinal);
        Assert.Contains("1 tasks are reassigned", shell.Confirms[^1], StringComparison.Ordinal);
        Assert.Null(vm.LeaverPreview);
    }

    [Fact]
    public async Task APreviewForOneUserCannotDisableAnother()
    {
        var (vm, gw, _) = Make();
        await vm.PreviewDisableAsync("ann");
        Assert.False(await vm.DisableAsync("bob"));
        Assert.DoesNotContain("Disable", gw.Calls);
    }

    [Fact]
    public async Task TheLastAdministratorMessageComesFromTheServerUnchanged()
    {
        var (vm, gw, _) = Make();
        await vm.PreviewDisableAsync("ann");
        gw.Failures["Disable"] = new GatewayError("iam.last_administrator", "Ann is the last administrator. Make someone else an administrator first.");
        Assert.False(await vm.DisableAsync("ann"));
        Assert.Equal("Ann is the last administrator. Make someone else an administrator first.", vm.Error);
    }

    [Fact]
    public async Task RecoveryPromptsForAStepUpAndRetriesWithTheProof()
    {
        var (vm, gw, shell) = Make();
        gw.RequireStepUp.Add("Recover");
        Assert.True(await vm.RecoverAsync("ann", RecoveryKind.ResetPassword));
        Assert.Equal(1, shell.StepUpPrompts);
        Assert.Equal([null, new StepUpProof("proof-1")], gw.ProofsSeen);
        Assert.Equal("Password reset sent.", vm.Status);
    }

    [Fact]
    public async Task DecliningTheStepUpCancelsWithoutAnError()
    {
        var (vm, gw, shell) = Make();
        gw.RequireStepUp.Add("Recover");
        shell.StepUp = null;
        Assert.False(await vm.RecoverAsync("ann", RecoveryKind.EndSessions));
        Assert.Null(vm.Error);
        Assert.Equal("Cancelled.", vm.Status);
        Assert.Single(gw.ProofsSeen);
    }

    [Fact]
    public async Task AnonymiseNeedsTheExactTypedConfirmation()
    {
        var (vm, gw, shell) = Make();
        var ann = Person("ann", "Ann Lee");
        shell.TextAnswer = "ann@acme.tes";
        Assert.False(await vm.AnonymiseAsync(ann));
        Assert.DoesNotContain("Anonymise", gw.Calls);
        Assert.Contains("does not match", vm.Error, StringComparison.Ordinal);

        shell.TextAnswer = null;
        Assert.False(await vm.AnonymiseAsync(ann));
        Assert.DoesNotContain("Anonymise", gw.Calls);

        shell.TextAnswer = " ann@acme.test ";
        gw.RequireStepUp.Add("Anonymise");
        Assert.True(await vm.AnonymiseAsync(ann));
        Assert.Contains("Anonymise", gw.Calls);
        Assert.Equal(1, shell.StepUpPrompts);
    }

    [Fact]
    public async Task KeyboardOnlyOnboardingIsAnOrderedCommandFlow()
    {
        // The whole onboarding runs through commands and method calls with no pointer: invite,
        // then the grant wizard, and each wizard step names its tab order.
        var (users, gw, shell) = Make();
        Assert.True(await users.InviteAsync(new InviteRequest("new@acme.test", "New Person", [])));
        var wizard = new AssignmentWizardViewModel(gw, shell) { UserId = "new", RoleId = "role-accountant" };
        Assert.Equal(["User", "Role", "Next"], wizard.FocusOrder);
        Assert.True(wizard.Next());
        Assert.Equal(["Company", "IncludeDescendants", "ValidFrom", "ValidTo", "Back", "Check"], wizard.FocusOrder);
        Assert.True(await wizard.RunCheckAsync(new DateOnly(2026, 10, 23)));
        Assert.Equal(["Back", "Confirm"], wizard.FocusOrder);
        Assert.True(await wizard.ConfirmAsync());
        Assert.Equal(["Close"], wizard.FocusOrder);
        Assert.Equal(GrantOutcome.Granted, wizard.Outcome);
    }
}
