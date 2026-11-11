using Spotledger.AdminCenter.Deploy;
using Spotledger.AdminCenter;
using Xunit;

namespace Spotledger.AdminCenter.Deploy.Tests;

public sealed class AddServerTests
{
    private static (AddServerWizardViewModel Wizard, FakeEngine Engine, FakeShell Shell) Make()
    {
        var engine = new FakeEngine();
        var shell = new FakeShell();
        return (new AddServerWizardViewModel(engine, shell) { Label = "db2" }, engine, shell);
    }

    [Fact]
    public async Task ATokenNeedsAServerName()
    {
        var (wizard, engine, _) = Make();
        wizard.Label = "  ";
        Assert.False(await wizard.CreateTokenAsync());
        Assert.NotNull(wizard.Error);
        Assert.DoesNotContain("CreateEnrolment", engine.Calls);
    }

    [Fact]
    public async Task TheTokenIsHandedOutAsCopyQrAndCommandLine()
    {
        var (wizard, _, shell) = Make();
        Assert.True(await wizard.CreateTokenAsync());
        Assert.Equal(AddServerStep.WaitingForAgent, wizard.Step);
        var handout = wizard.Handout!;
        Assert.Contains(handout.Token, handout.CommandLine, StringComparison.Ordinal);
        Assert.Contains(handout.Token, handout.QrPayload, StringComparison.Ordinal);
        Assert.True(await wizard.CopyCommandAsync());
        Assert.Equal(handout.CommandLine, Assert.Single(shell.Copied));
    }

    [Fact]
    public async Task AUsedTokenIsNeverShownAgain()
    {
        var (wizard, engine, shell) = Make();
        await wizard.CreateTokenAsync();
        engine.Enrolment = new EnrolmentStatus(EnrolmentState.Used, FakeEngine.Facts("db2.example.com"));
        Assert.True(await wizard.PollAsync(10));
        Assert.Null(wizard.Handout);
        Assert.Equal(AddServerStep.Facts, wizard.Step);
        Assert.Equal("db2.example.com", wizard.Facts?.Host);
        Assert.False(await wizard.CopyCommandAsync());
        Assert.Empty(shell.Copied);
    }

    [Fact]
    public async Task ATokenUsedBeforeTheAgentReportsIsHiddenAtOnce()
    {
        var (wizard, engine, _) = Make();
        await wizard.CreateTokenAsync();
        engine.Enrolment = new EnrolmentStatus(EnrolmentState.Used, null);
        Assert.False(await wizard.PollAsync(10));
        Assert.Null(wizard.Handout);
        Assert.Equal(AddServerStep.WaitingForAgent, wizard.Step);
    }

    [Fact]
    public async Task AnExpiredTokenIsHiddenAndAnotherCanBeCreated()
    {
        var (wizard, _, _) = Make();
        await wizard.CreateTokenAsync();
        Assert.False(await wizard.PollAsync(1_001));
        Assert.Null(wizard.Handout);
        Assert.Equal(AddServerStep.Token, wizard.Step);
        Assert.Contains("expired", wizard.Error, StringComparison.Ordinal);
        Assert.True(await wizard.CreateTokenAsync());
        Assert.Equal("tok-secret-2", wizard.Handout?.Token);
    }

    [Fact]
    public async Task TheSecretIsNeverPrinted()
    {
        var (wizard, engine, _) = Make();
        await wizard.CreateTokenAsync();
        Assert.DoesNotContain("tok-secret", wizard.Handout!.ToString(), StringComparison.Ordinal);
        var offer = await engine.CreateEnrolmentAsync("x", CancellationToken.None);
        Assert.DoesNotContain("tok-secret", offer.ToString(), StringComparison.Ordinal);
    }

    [Fact]
    public async Task AFailingPreflightRowBlocksFinishingUntilItIsFixed()
    {
        var (wizard, engine, _) = Make();
        await wizard.CreateTokenAsync();
        engine.Enrolment = new EnrolmentStatus(EnrolmentState.Used, FakeEngine.Facts("db2.example.com"));
        await wizard.PollAsync(10);
        engine.PreflightFor["db2.example.com"] = Severity.Fail;
        Assert.True(await wizard.RunPreflightAsync());
        Assert.False(wizard.CanFinish);
        Assert.Equal(Severity.Fail, wizard.Rows[0].Severity);
        Assert.Contains("20 GB", wizard.Rows[0].FixHint, StringComparison.Ordinal);
        Assert.Equal("1 failed, 0 to look at, 1 passed", wizard.Summary);
        Assert.False(wizard.Finish());
        Assert.Equal(AddServerStep.Preflight, wizard.Step);

        engine.PreflightFor["db2.example.com"] = Severity.Pass;
        await wizard.RunPreflightAsync();
        Assert.True(wizard.Finish());
        Assert.Equal(AddServerStep.Done, wizard.Step);
    }

    [Fact]
    public async Task AWarningDoesNotBlock()
    {
        var (wizard, engine, _) = Make();
        await wizard.CreateTokenAsync();
        engine.Enrolment = new EnrolmentStatus(EnrolmentState.Used, FakeEngine.Facts("db2.example.com"));
        await wizard.PollAsync(10);
        engine.PreflightFor["db2.example.com"] = Severity.Warn;
        await wizard.RunPreflightAsync();
        Assert.True(wizard.CanFinish);
    }

    [Fact]
    public async Task KeyboardOnlyCompletionVisitsEveryControlInOrder()
    {
        var (wizard, engine, _) = Make();
        Assert.Equal(["Label", "CreateToken"], wizard.FocusOrder);
        await wizard.CreateTokenAsync();
        Assert.Contains("CopyCommand", wizard.FocusOrder);
        engine.Enrolment = new EnrolmentStatus(EnrolmentState.Used, FakeEngine.Facts("db2.example.com"));
        await wizard.PollAsync(10);
        Assert.Equal(["RunPreflight"], wizard.FocusOrder);
        await wizard.RunPreflightAsync();
        Assert.Equal(["RunPreflightAgain", "Finish"], wizard.FocusOrder);
        Assert.True(wizard.Finish());
        Assert.Equal(["Close"], wizard.FocusOrder);
    }

    [Fact]
    public void TheToolsRegisterForInfrastructureAdministratorsOnly()
    {
        var registry = new Spotledger.AdminCenter.ToolRegistry();
        DeployTools.Register(registry);
        var infra = new AdminPrincipal("ops", new HashSet<AdminRole> { AdminRole.InfrastructureAdministrator });
        var erp = new AdminPrincipal("fin", new HashSet<AdminRole> { AdminRole.ErpAdministrator });
        var entitlements = new Entitlements("Enterprise", new HashSet<string>());
        Assert.Equal(2, registry.Navigation(infra, entitlements).Count);
        Assert.Empty(registry.Navigation(erp, entitlements));
        Assert.Throws<InvalidOperationException>(() => DeployTools.Register(registry));
    }
}
