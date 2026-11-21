using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Servers;
using Xunit;

namespace Spotledger.AdminCenter.Servers.Tests;

public sealed class OperationTests
{
    private static readonly PlanPreview LockoutPreview = new(
        Ops.FirewallClose, "Close port 22.", PreviewRisk.Critical, [], [], ["tcp/22 allow"], true,
        ["This rule carries your current connection. Closing it will lock you out."]);

    [Fact]
    public async Task NothingIsSentToTheServerBeforeThePreviewIsShown()
    {
        var api = new FakeServerApi();
        var screen = new ServicesScreen(Fixtures.Context(api));
        Assert.False(await screen.RunOperationAsync(Fixtures.Now));
        await screen.StartAsync("cups");
        Assert.Equal(["preview os.service.start"], api.Calls);
        Assert.Equal(OperationPhase.Review, screen.Operation!.Phase);
        Assert.Empty(api.Ran);
    }

    [Fact]
    public async Task AnOperationNeedingNoSignatureStillRunsOnlyAfterItsPreview()
    {
        var api = new FakeServerApi();
        var screen = new ServicesScreen(Fixtures.Context(api));
        await screen.StartAsync("cups");
        var op = screen.Operation!;
        Assert.Equal(0, op.Signatures!.Required);
        Assert.True(op.CanRun);
        Assert.True(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Equal(OperationPhase.Succeeded, op.Phase);
        Assert.Equal(["Applying...", "Verifying..."], op.Progress);
        Assert.True(op.Result!.Verify!.Ok);
        Assert.Contains("os.service.start", op.EquivalentCommandText, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ThePreviewRendersAddsChangesRemovesRiskAndReversibility()
    {
        var api = new FakeServerApi();
        api.Previews[Ops.FirewallOpen] = new PlanPreview(
            Ops.FirewallOpen, "Open 8443.", PreviewRisk.Medium, ["tcp/8443 allow"], [new PreviewChange("default", "deny", "allow")], ["tcp/8080 allow"], false, []);
        var screen = new FirewallScreen(Fixtures.Context(api));
        await screen.AddAsync(8443, "tcp", "Edge");
        var dialog = screen.Operation!.Preview!;
        Assert.Equal(["+ tcp/8443 allow", "~ default: deny -> allow", "- tcp/8080 allow"], dialog.Lines);
        Assert.StartsWith("Medium risk", dialog.RiskText, StringComparison.Ordinal);
        Assert.Equal("Cannot be undone.", dialog.ReversibleText);
        Assert.Equal("Open 8443.", dialog.Summary);
    }

    [Fact]
    public async Task TheLockoutWarningMustBeAcknowledgedBeforeTheFirewallChangeRuns()
    {
        var api = new FakeServerApi();
        api.Previews[Ops.FirewallClose] = LockoutPreview;
        var screen = new FirewallScreen(Fixtures.Context(api));
        await screen.RemoveAsync(new FirewallRule("r1", "SSH", 22, "tcp", true));
        var op = screen.Operation!;
        Assert.Single(op.Preview!.Warnings);
        await op.Signatures!.ApproveAsync(new FakeSigner("ann"));
        Assert.True(op.Signatures.IsSatisfied);
        Assert.False(op.CanRun);
        Assert.False(await op.RunAsync());
        Assert.Contains("acknowledge", op.Error, StringComparison.Ordinal);
        Assert.Empty(api.Ran);

        op.AcknowledgeWarnings();
        Assert.True(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Equal("ann", Assert.Single(Assert.Single(api.ApprovalsSeen)).SignerId);
    }

    [Fact]
    public async Task ATwoPersonOperationStaysPendingUntilADifferentPersonSigns()
    {
        var api = new FakeServerApi();
        var screen = new StorageScreen(Fixtures.Context(api));
        await screen.RestoreSnapshotAsync("s1");
        var op = screen.Operation!;
        var panel = op.Signatures!;
        Assert.True(panel.IsPending);
        Assert.Equal("Waiting for your signature", panel.StatusText);
        Assert.False(await op.RunAsync());
        Assert.Contains("signatures", op.Error, StringComparison.Ordinal);

        Assert.True(await panel.ApproveAsync(new FakeSigner("ann")));
        Assert.Equal("Waiting for second approver", panel.StatusText);
        Assert.False(await panel.ApproveAsync(new FakeSigner("ann")));
        Assert.Contains("different person", panel.Error, StringComparison.Ordinal);
        Assert.False(op.CanRun);

        Assert.True(await panel.ApproveAsync(new FakeSigner("bob")));
        Assert.Equal(["ann", "bob"], panel.Signers);
        Assert.True(panel.IsSatisfied);
        Assert.Contains("op=os.snapshot.restore", panel.SignedText, StringComparison.Ordinal);
        Assert.True(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Equal(2, api.ApprovalsSeen[0].Count);
    }

    [Fact]
    public async Task AnEmptyPlanMeansNothingToDoAndNothingRuns()
    {
        var api = new FakeServerApi();
        api.Previews[Ops.UserLock] = new PlanPreview(Ops.UserLock, "bob is already locked.", PreviewRisk.Low, [], [], [], true, []);
        var screen = new UsersScreen(Fixtures.Context(api));
        Assert.False(await screen.LockAsync("bob"));
        Assert.Equal(OperationPhase.NothingToDo, screen.Operation!.Phase);
        Assert.Contains("Nothing to change", screen.Error, StringComparison.Ordinal);
        Assert.False(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Empty(api.Ran);
    }

    [Fact]
    public async Task AFailedVerifyCheckIsReportedAsFailureEvenIfTheCommandRan()
    {
        var api = new FakeServerApi { RunResult = new OperationResult(true, "Done.", new VerifyCheck(false, "service still stopped"), null) };
        var screen = new ServicesScreen(Fixtures.Context(api));
        await screen.StartAsync("cups");
        Assert.False(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Equal(OperationPhase.Failed, screen.Operation!.Phase);
        Assert.Contains("service still stopped", screen.Operation.Error, StringComparison.Ordinal);
    }

    [Fact]
    public async Task AServerRefusalDuringTheRunIsShownInItsOwnWords()
    {
        var api = new FakeServerApi { RefuseRun = "Signatures do not satisfy the operation." };
        var screen = new ServicesScreen(Fixtures.Context(api));
        await screen.StartAsync("cups");
        Assert.False(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Equal("Signatures do not satisfy the operation.", screen.Operation!.Error);
    }

    [Fact]
    public async Task AnOperationTheAgentDoesNotHaveIsRefusedBeforeAnyPreview()
    {
        var api = new FakeServerApi { Capabilities = Fixtures.Windows };
        var screen = new StorageScreen(new ServerContext(api, Fixtures.Commands, Fixtures.Windows));
        Assert.False(await screen.CreateSnapshotAsync("data", "x"));
        Assert.Equal("This server does not support that action.", screen.Error);
        Assert.Empty(api.Calls);
        Assert.Null(screen.Operation);
    }

    [Fact]
    public async Task ADismissedOperationLeavesNothingBehind()
    {
        var api = new FakeServerApi();
        var screen = new ServicesScreen(Fixtures.Context(api));
        await screen.StartAsync("cups");
        screen.DismissOperation();
        Assert.Null(screen.Operation);
        Assert.Empty(api.Ran);
    }

    [Fact]
    public async Task ANetworkChangeCountsDownAndConfirmsInTime()
    {
        var api = new FakeServerApi { RunResult = new OperationResult(true, "Applied.", new VerifyCheck(true, "ok"), new RevertTicket("t1", Fixtures.Now + 60)) };
        api.Previews[Ops.NetworkSet] = new PlanPreview(Ops.NetworkSet, "Use 10.0.0.9.", PreviewRisk.High, [], [new PreviewChange("address", "10.0.0.5/24", "10.0.0.9/24")], [], true, [], RevertWindowSeconds: 60);
        var screen = new NetworkScreen(Fixtures.Context(api));
        await screen.LoadAsync(Fixtures.Now);
        Assert.False(await screen.UseStaticAsync("eth0", " ", "10.0.0.1"));
        Assert.True(await screen.UseStaticAsync("eth0", "10.0.0.9/24", "10.0.0.1"));
        var op = screen.Operation!;
        Assert.Contains("60 seconds", op.Preview!.ReversibleText, StringComparison.Ordinal);
        await op.Signatures!.ApproveAsync(new FakeSigner("ann"));
        await op.Signatures.ApproveAsync(new FakeSigner("bob"));
        Assert.True(await screen.RunOperationAsync(Fixtures.Now));
        Assert.Equal(OperationPhase.AwaitingConfirmation, op.Phase);

        screen.Tick(Fixtures.Now + 20);
        Assert.Equal(40, op.RemainingSeconds);
        Assert.True(await screen.ConfirmAsync(Fixtures.Now + 21));
        Assert.Equal(OperationPhase.Succeeded, op.Phase);
        Assert.Equal("t1", Assert.Single(api.Confirmed).Id);
    }

    [Fact]
    public async Task ANetworkChangeNotConfirmedInTimeIsReportedAsReverted()
    {
        var api = new FakeServerApi { RunResult = new OperationResult(true, "Applied.", new VerifyCheck(true, "ok"), new RevertTicket("t1", Fixtures.Now + 60)) };
        var screen = new NetworkScreen(Fixtures.Context(api));
        await screen.UseDhcpAsync("eth0");
        var op = screen.Operation!;
        await op.Signatures!.ApproveAsync(new FakeSigner("ann"));
        await op.Signatures.ApproveAsync(new FakeSigner("bob"));
        await screen.RunOperationAsync(Fixtures.Now);

        screen.Tick(Fixtures.Now + 61);
        Assert.Equal(OperationPhase.Reverted, op.Phase);
        Assert.Equal(0, op.RemainingSeconds);
        Assert.Contains("reverted", op.Error, StringComparison.Ordinal);
        Assert.False(await screen.ConfirmAsync(Fixtures.Now + 62));
        Assert.Empty(api.Confirmed);
    }

    [Fact]
    public async Task AConfirmationAfterTheDeadlineIsRefusedLocally()
    {
        var api = new FakeServerApi { RunResult = new OperationResult(true, "Applied.", new VerifyCheck(true, "ok"), new RevertTicket("t1", Fixtures.Now + 60)) };
        var screen = new NetworkScreen(Fixtures.Context(api));
        await screen.UseDhcpAsync("eth0");
        await screen.Operation!.Signatures!.ApproveAsync(new FakeSigner("ann"));
        await screen.Operation.Signatures.ApproveAsync(new FakeSigner("bob"));
        await screen.RunOperationAsync(Fixtures.Now);
        Assert.False(await screen.ConfirmAsync(Fixtures.Now + 90));
        Assert.Equal(OperationPhase.Reverted, screen.Operation.Phase);
    }

    [Fact]
    public async Task AnOperationNotInTheCatalogueFailsClosed()
    {
        var api = new FakeServerApi();
        var context = new ServerContext(api, new EquivalentCommand([]), Fixtures.Linux);
        var screen = new ServicesScreen(context);
        Assert.False(await screen.StartAsync("cups"));
        Assert.Equal(OperationPhase.Failed, screen.Operation!.Phase);
        Assert.NotNull(screen.Error);
        Assert.Empty(api.Ran);
    }
}

public sealed class CapabilityTests
{
    [Fact]
    public void ALinuxServerGetsTheLinuxSetAndAWindowsServerTheWindowsSetFromCapabilities()
    {
        Assert.Equal(
            [ServerTools.OverviewId, ServerTools.ServicesId, ServerTools.PackagesId, ServerTools.UsersId, ServerTools.FirewallId, ServerTools.StorageId, ServerTools.NetworkId, ServerTools.CertificatesId],
            ServerTools.AvailableFor(Fixtures.Linux));
        Assert.Equal(
            [ServerTools.OverviewId, ServerTools.ServicesId, ServerTools.PackagesId, ServerTools.UsersId, ServerTools.FirewallId],
            ServerTools.AvailableFor(Fixtures.Windows));
    }

    [Fact]
    public void TheSetFollowsCapabilitiesNotTheOsName()
    {
        var odd = new ServerCapabilities(OsFamily.Windows, new HashSet<string> { Ops.NetworkSet, Ops.CertInstall });
        Assert.Equal([ServerTools.OverviewId, ServerTools.NetworkId, ServerTools.CertificatesId], ServerTools.AvailableFor(odd));
    }

    [Fact]
    public void TheToolsRegisterForInfrastructureAdministratorsOnly()
    {
        var registry = new ToolRegistry();
        ServerTools.Register(registry);
        var infra = new AdminPrincipal("ops", new HashSet<AdminRole> { AdminRole.InfrastructureAdministrator });
        var erp = new AdminPrincipal("fin", new HashSet<AdminRole> { AdminRole.ErpAdministrator });
        var entitlements = new Entitlements("Enterprise", new HashSet<string>());
        Assert.Equal(8, registry.Navigation(infra, entitlements).Count);
        Assert.Empty(registry.Navigation(erp, entitlements));
    }
}
