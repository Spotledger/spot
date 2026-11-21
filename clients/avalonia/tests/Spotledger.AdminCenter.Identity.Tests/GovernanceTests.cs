using Spotledger.AdminCenter.Identity;
using Xunit;

namespace Spotledger.AdminCenter.Identity.Tests;

public sealed class GovernanceTests
{
    private static readonly DateOnly Today = new(2026, 10, 23);

    [Fact]
    public async Task RequiringSsoIsRefusedWithoutTwoBreakGlassAdministrators()
    {
        var gw = new FakeGateway { Tenant = new TenantAuthState(SsoPolicy.Optional, 1) };
        gw.Providers.Add(new SsoProvider("p1", "Okta", SsoProtocol.Oidc, "https://idp.test", "cid", null, true));
        var vm = new SsoViewModel(gw, new FakeShell());
        await vm.RefreshAsync();
        Assert.Contains("1 break-glass", vm.BreakGlassWarning, StringComparison.Ordinal);

        Assert.False(await vm.SetPolicyAsync(SsoPolicy.Required));
        Assert.Contains("at least 2", vm.Error, StringComparison.Ordinal);
        Assert.DoesNotContain("SetPolicy", gw.Calls);

        gw.Tenant = new TenantAuthState(SsoPolicy.Optional, 2);
        Assert.True(await vm.SetPolicyAsync(SsoPolicy.Required));
        Assert.Equal(SsoPolicy.Required, vm.Tenant.Policy);
        Assert.Null(vm.BreakGlassWarning);
    }

    [Fact]
    public async Task RequiringSsoNeedsAProviderAndAConfirmation()
    {
        var gw = new FakeGateway { Tenant = new TenantAuthState(SsoPolicy.Optional, 3) };
        var shell = new FakeShell();
        var vm = new SsoViewModel(gw, shell);
        await vm.RefreshAsync();
        Assert.False(await vm.SetPolicyAsync(SsoPolicy.Required));
        gw.Providers.Add(new SsoProvider("p1", "Okta", SsoProtocol.Oidc, "https://idp.test", "cid", null, true));
        await vm.RefreshAsync();
        shell.ConfirmAnswer = false;
        Assert.False(await vm.SetPolicyAsync(SsoPolicy.Required));
        Assert.DoesNotContain("SetPolicy", gw.Calls);
        Assert.True(await vm.SetPolicyAsync(SsoPolicy.Local));
    }

    [Fact]
    public async Task ProviderFormsAreValidatedAndTheSecretIsWriteOnly()
    {
        var gw = new FakeGateway();
        var vm = new SsoViewModel(gw, new FakeShell());
        Assert.False(await vm.SaveProviderAsync(new SsoProviderDraft(null, "Okta", SsoProtocol.Oidc, "http://idp.test", "cid", null, "s")));
        Assert.False(await vm.SaveProviderAsync(new SsoProviderDraft(null, "Okta", SsoProtocol.Oidc, "https://idp.test", "cid", null, null)));
        Assert.False(await vm.SaveProviderAsync(new SsoProviderDraft(null, "", SsoProtocol.Saml, "https://idp.test", "sp", null, null)));
        Assert.DoesNotContain("SaveProvider", gw.Calls);

        var draft = new SsoProviderDraft(null, "Okta", SsoProtocol.Oidc, "https://idp.test", "cid", null, "TOP-SECRET");
        Assert.DoesNotContain("TOP-SECRET", draft.ToString(), StringComparison.Ordinal);
        Assert.True(await vm.SaveProviderAsync(draft));
        Assert.True(vm.Providers[0].HasSecret);
        Assert.DoesNotContain("TOP-SECRET", vm.Status, StringComparison.Ordinal);

        Assert.True(await vm.SaveProviderAsync(new SsoProviderDraft(null, "Entra", SsoProtocol.Saml, "https://login.test/entity", "sp-entity", "https://login.test/meta", null)));
    }

    [Fact]
    public async Task ATestSignInReportsSuccessAndFailure()
    {
        var gw = new FakeGateway();
        var vm = new SsoViewModel(gw, new FakeShell());
        Assert.True(await vm.TestAsync("p1"));
        gw.TestResult = new SsoTestResult(false, "The issuer did not answer.");
        Assert.False(await vm.TestAsync("p1"));
        Assert.Equal("The issuer did not answer.", vm.LastTest?.Message);
        Assert.Equal("Test sign-in failed.", vm.Status);
    }

    [Fact]
    public async Task ScimTokenIsShownOnceAfterAStepUpAndNeverListed()
    {
        var gw = new FakeGateway();
        gw.RequireStepUp.Add("CreateScimToken");
        var shell = new FakeShell();
        var vm = new ScimViewModel(gw, shell);
        Assert.True(await vm.CreateTokenAsync());
        Assert.Equal(1, shell.StepUpPrompts);
        Assert.Equal("scim_TOKEN_SECRET", vm.RevealedSecret?.Value);
        Assert.Single(vm.Tokens);
        Assert.DoesNotContain("TOKEN_SECRET", vm.Tokens[0].ToString(), StringComparison.Ordinal);
        Assert.DoesNotContain("TOKEN_SECRET", vm.Status, StringComparison.Ordinal);
        vm.DismissSecret();
        Assert.Null(vm.RevealedSecret);
        await vm.RefreshAsync();
        Assert.Null(vm.RevealedSecret);
    }

    [Fact]
    public async Task RevokingAScimTokenNeedsConfirmationAndStepUp()
    {
        var gw = new FakeGateway();
        gw.ScimTokens.Add(new ScimTokenInfo("t0", new DateTimeOffset(2026, 10, 1, 0, 0, 0, TimeSpan.Zero), null));
        var shell = new FakeShell { ConfirmAnswer = false };
        var vm = new ScimViewModel(gw, shell);
        await vm.RefreshAsync();
        Assert.False(await vm.RevokeTokenAsync("t0"));
        Assert.DoesNotContain("RevokeScimToken", gw.Calls);
        shell.ConfirmAnswer = true;
        Assert.True(await vm.RevokeTokenAsync("t0"));
        Assert.Empty(vm.Tokens);
    }

    [Fact]
    public async Task UnmappedGroupsAreWarnedAboutUntilTheyAreMapped()
    {
        var gw = new FakeGateway();
        gw.UnmappedGroups.AddRange(["finance-ap", "it-ops"]);
        var vm = new ScimViewModel(gw, new FakeShell());
        await vm.RefreshAsync();
        Assert.Contains("2 group(s)", vm.UnmappedWarning, StringComparison.Ordinal);
        Assert.Contains("finance-ap", vm.UnmappedWarning, StringComparison.Ordinal);

        Assert.True(vm.AddMapping(new GroupMapping("finance-ap", "role-ap", new AssignmentScope("acme-pk", true))));
        Assert.Contains("1 group(s)", vm.UnmappedWarning, StringComparison.Ordinal);
        Assert.True(vm.HasUnsavedMappings);
        Assert.True(await vm.SaveMappingsAsync());
        Assert.False(vm.HasUnsavedMappings);
        Assert.Single(vm.Mappings);
        Assert.Single(vm.UnmappedGroups);
    }

    [Fact]
    public void MappingRowsNeedAGroupAndARoleAndRejectDuplicates()
    {
        var vm = new ScimViewModel(new FakeGateway(), new FakeShell());
        var scope = new AssignmentScope(null, false);
        Assert.False(vm.AddMapping(new GroupMapping("", "r", scope)));
        Assert.False(vm.AddMapping(new GroupMapping("g", "", scope)));
        Assert.True(vm.AddMapping(new GroupMapping("g", "r", scope)));
        Assert.False(vm.AddMapping(new GroupMapping("G", "r", scope)));
        Assert.True(vm.RemoveMapping(vm.Mappings[0]));
        Assert.Empty(vm.Mappings);
    }

    private static ReviewItem Item(string id, ReviewDecision decision = ReviewDecision.Undecided, bool sod = false, AssignmentSource source = AssignmentSource.Manual) =>
        new(id, new AssignmentInfo("a-" + id, "ann", "role-ap", new AssignmentScope(null, false), null, null, source), "Ann", "AP clerk", new DateOnly(2026, 5, 1), sod, decision);

    [Fact]
    public async Task ACampaignNeedsANameAReviewerAndAFutureDueDate()
    {
        var gw = new FakeGateway();
        var vm = new AccessReviewViewModel(gw, new FakeShell());
        var scope = new AssignmentScope(null, false);
        Assert.False(await vm.CreateCampaignAsync(new CampaignRequest("", scope, ["bob"], Today.AddDays(7)), Today));
        Assert.False(await vm.CreateCampaignAsync(new CampaignRequest("Q4", scope, [], Today.AddDays(7)), Today));
        Assert.False(await vm.CreateCampaignAsync(new CampaignRequest("Q4", scope, ["bob"], Today), Today));
        Assert.True(await vm.CreateCampaignAsync(new CampaignRequest("Q4", scope, ["bob"], Today.AddDays(7)), Today));
        Assert.Single(vm.Campaigns);
    }

    [Fact]
    public async Task AReviewerKeepsAndRemovesAndTheReportCountsThem()
    {
        var gw = new FakeGateway();
        gw.Campaigns.Add(new CampaignInfo("c0", "Q4", Today.AddDays(7), 3, 0, "Reminder sent to bob", false));
        gw.Worklist.AddRange([Item("1"), Item("2", sod: true), Item("3")]);
        var vm = new AccessReviewViewModel(gw, new FakeShell());
        await vm.RefreshAsync();
        Assert.True(await vm.OpenAsync("c0"));
        Assert.Equal("0 of 3 decided", vm.Progress);
        Assert.Equal("Reminder sent to bob", vm.Escalation);
        Assert.True(vm.Worklist[1].SodOverridden);
        Assert.Equal(new DateOnly(2026, 5, 1), vm.Worklist[0].LastUsed);

        Assert.True(vm.Decide("1", ReviewDecision.Keep));
        Assert.True(vm.Decide("2", ReviewDecision.Keep));
        Assert.Equal("2 of 3 decided", vm.Progress);
        Assert.False(await vm.CompleteAsync());
        Assert.Contains("1 assignment(s) still need a decision", vm.Error, StringComparison.Ordinal);
        Assert.DoesNotContain("CompleteCampaign", gw.Calls);

        Assert.True(vm.Decide("3", ReviewDecision.Remove));
        Assert.True(await vm.CompleteAsync());
        Assert.Equal(new ReviewReport("c0", 2, 1, 0, 1), vm.Report);
        Assert.Equal(3, gw.LastDecisions?.Count);
    }

    [Fact]
    public async Task AnIdpManagedAssignmentCannotBeRemovedInAReview()
    {
        var gw = new FakeGateway();
        gw.Worklist.Add(Item("1", source: AssignmentSource.Idp));
        var vm = new AccessReviewViewModel(gw, new FakeShell());
        await vm.OpenAsync("c0");
        Assert.False(vm.Decide("1", ReviewDecision.Remove));
        Assert.Contains("identity provider", vm.Error, StringComparison.Ordinal);
        Assert.True(vm.Decide("1", ReviewDecision.Keep));
        Assert.False(vm.Decide("zzz", ReviewDecision.Keep));
    }

    [Fact]
    public async Task SavedDecisionsSurviveAReload()
    {
        var gw = new FakeGateway();
        gw.Worklist.AddRange([Item("1"), Item("2")]);
        var vm = new AccessReviewViewModel(gw, new FakeShell());
        await vm.OpenAsync("c0");
        vm.Decide("1", ReviewDecision.Remove);
        Assert.True(await vm.SaveDecisionsAsync());
        Assert.Equal("1 of 2 decided", vm.Progress);
        Assert.Equal(ReviewDecision.Remove, vm.DecisionOf(vm.Worklist[0]));
    }
}
