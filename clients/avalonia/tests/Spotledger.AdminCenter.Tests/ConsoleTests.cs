using System.Text;
using Xunit;

namespace Spotledger.AdminCenter.Tests;

public sealed class NavigationTests
{
    [Fact]
    public void InfrastructureOnlyPrincipalSeesNoErpTools()
    {
        var items = Fixture.Registry().Navigation(Fixture.InfraOnly, Fixture.Enterprise);
        Assert.Equal(["server.nodes", "fleet.rollout"], items.Select(i => i.Tool.Id));
    }

    [Fact]
    public void ErpOnlyPrincipalSeesNoInfrastructureTools()
    {
        var items = Fixture.Registry().Navigation(Fixture.ErpOnly, Fixture.Enterprise);
        Assert.Equal(["tenant.users"], items.Select(i => i.Tool.Id));
    }

    [Fact]
    public void NeitherRoleImpliesTheOther()
    {
        var none = new AdminPrincipal("nobody", new HashSet<AdminRole>());
        Assert.Empty(Fixture.Registry().Navigation(none, Fixture.Enterprise));
    }

    [Fact]
    public void MissingEditionCapabilityDisablesWithTooltip()
    {
        var rollout = Fixture.Registry().Navigation(Fixture.InfraOnly, Fixture.Community).Single(i => i.Tool.Id == "fleet.rollout");
        Assert.False(rollout.Enabled);
        Assert.Contains("fleet", rollout.Tooltip, StringComparison.Ordinal);
        Assert.Contains("Community", rollout.Tooltip, StringComparison.Ordinal);
    }

    [Fact]
    public void GrantedCapabilityEnablesEntry()
    {
        var rollout = Fixture.Registry().Navigation(Fixture.InfraOnly, Fixture.Enterprise).Single(i => i.Tool.Id == "fleet.rollout");
        Assert.True(rollout.Enabled);
        Assert.Null(rollout.Tooltip);
    }

    [Fact]
    public void DuplicateToolIdIsRefused()
    {
        var registry = Fixture.Registry();
        Assert.Throws<InvalidOperationException>(() =>
            registry.Register(new ToolEntry("server.nodes", "Again", ToolCategory.Server, AdminRole.InfrastructureAdministrator)));
    }

    [Fact]
    public void SigningInAgainRecomputesNavigation()
    {
        var model = new AdminConsoleViewModel(Fixture.Registry(), new EquivalentCommand(Fixture.Catalogue), Fixture.InfraOnly, Fixture.Enterprise);
        var changed = new List<string?>();
        model.PropertyChanged += (_, e) => changed.Add(e.PropertyName);
        model.SignIn(Fixture.ErpOnly, Fixture.Enterprise);
        Assert.Contains(nameof(AdminConsoleViewModel.Navigation), changed);
        Assert.Equal(["tenant.users"], model.Navigation.Select(i => i.Tool.Id));
    }
}

public sealed class ConnectionTests
{
    private static ConnectionProfile Profile(string id = "a", string? pin = null) =>
        new(id, "Prod " + id, new Uri("https://mgmt.example.test:8443/"), "THUMB", pin, "prod", ["eu", "primary"]);

    [Fact]
    public async Task FirstConnectPromptsThenPins()
    {
        var store = new MemoryCredentialStore();
        var manager = new ConnectionManager(store);
        await manager.AddAsync(Profile(), CancellationToken.None);
        var prompt = new FakePrompt(true);

        var result = await manager.ConnectAsync("a", new FakeProbe("FP1"), prompt, CancellationToken.None);

        Assert.Equal(ConnectOutcome.Connected, result.Outcome);
        Assert.Equal(1, prompt.Asked);
        Assert.Equal("FP1", manager.Profiles.Single().PinnedFingerprint);
        Assert.Equal(HealthBadge.Healthy, manager.HealthOf("a"));

        var again = await manager.ConnectAsync("a", new FakeProbe("FP1"), prompt, CancellationToken.None);
        Assert.Equal(ConnectOutcome.Connected, again.Outcome);
        Assert.Equal(1, prompt.Asked);
    }

    [Fact]
    public async Task DecliningThePinDoesNotSaveIt()
    {
        var manager = new ConnectionManager(new MemoryCredentialStore());
        await manager.AddAsync(Profile(), CancellationToken.None);
        var result = await manager.ConnectAsync("a", new FakeProbe("FP1"), new FakePrompt(false), CancellationToken.None);
        Assert.Equal(ConnectOutcome.PinRejectedByUser, result.Outcome);
        Assert.Null(manager.Profiles.Single().PinnedFingerprint);
    }

    [Fact]
    public async Task ChangedCertificateIsRefusedWithoutPrompting()
    {
        var manager = new ConnectionManager(new MemoryCredentialStore());
        await manager.AddAsync(Profile(pin: "FP1"), CancellationToken.None);
        var prompt = new FakePrompt(true);
        var result = await manager.ConnectAsync("a", new FakeProbe("EVIL"), prompt, CancellationToken.None);
        Assert.Equal(ConnectOutcome.PinMismatch, result.Outcome);
        Assert.Equal(0, prompt.Asked);
        Assert.Equal("FP1", manager.Profiles.Single().PinnedFingerprint);
    }

    [Fact]
    public async Task UnreachableServerShowsUnreachableBadge()
    {
        var manager = new ConnectionManager(new MemoryCredentialStore());
        await manager.AddAsync(Profile(), CancellationToken.None);
        var result = await manager.ConnectAsync("a", new FakeProbe("x", fail: true), new FakePrompt(true), CancellationToken.None);
        Assert.Equal(ConnectOutcome.Unreachable, result.Outcome);
        Assert.Equal(HealthBadge.Unreachable, manager.HealthOf("a"));
    }

    [Fact]
    public async Task ProfilesSurviveReloadThroughTheCredentialStoreOnly()
    {
        var store = new MemoryCredentialStore();
        var manager = new ConnectionManager(store);
        await manager.AddAsync(Profile("a"), CancellationToken.None);
        await manager.AddAsync(Profile("b", pin: "FP"), CancellationToken.None);
        Assert.Single(store.Items);

        var reloaded = new ConnectionManager(store);
        await reloaded.LoadAsync(CancellationToken.None);
        Assert.Equal(2, reloaded.Profiles.Count);
        Assert.Equal(["prod"], reloaded.Groups);
        Assert.Equal(2, reloaded.WithTag("eu").Count);
        Assert.Equal(2, reloaded.InGroup("prod").Count);
    }

    [Fact]
    public async Task ChangingTheEndpointDropsThePin()
    {
        var manager = new ConnectionManager(new MemoryCredentialStore());
        await manager.AddAsync(Profile(pin: "FP1"), CancellationToken.None);
        await manager.UpdateAsync(Profile(pin: "FP1") with { Endpoint = new Uri("https://other.example.test/") }, CancellationToken.None);
        Assert.Null(manager.Profiles.Single().PinnedFingerprint);
        await manager.RemoveAsync("a", CancellationToken.None);
        Assert.Empty(manager.Profiles);
    }
}

public sealed class ActionTests
{
    private static readonly EquivalentCommand Commands = new(Fixture.Catalogue);

    [Fact]
    public void EveryCatalogueOperationRendersACommandAndPreview()
    {
        foreach (var spec in Fixture.Catalogue)
        {
            var action = new AdminAction(spec.OperationId, [.. spec.Arguments.Select(a => new KeyValuePair<string, string>(a.Name, "v 1"))]);
            var cli = Commands.Cli(action);
            Assert.StartsWith("spot admin " + spec.OperationId, cli, StringComparison.Ordinal);
            Assert.Contains("op=" + spec.OperationId, Commands.SignedCommandPreview(action), StringComparison.Ordinal);
        }
    }

    [Fact]
    public void SecretArgumentsAreNeverRendered()
    {
        var action = AdminAction.Create("tenant.create", ("name", "acme"), ("admin-password", "hunter2"));
        Assert.Equal("spot admin tenant.create --name acme --admin-password '***'", Commands.Cli(action));
        Assert.DoesNotContain("hunter2", Commands.SignedCommandPreview(action), StringComparison.Ordinal);
    }

    [Fact]
    public void ArgumentsWithSpacesAndQuotesAreShellQuoted()
    {
        var action = AdminAction.Create("server.restart", ("node", "it's a node"));
        Assert.Equal("spot admin server.restart --node 'it'\\''s a node'", Commands.Cli(action));
    }

    [Fact]
    public void PreviewIsStableUnderArgumentOrder()
    {
        var a = AdminAction.Create("tenant.create", ("name", "x"), ("admin-password", "p"));
        var b = AdminAction.Create("tenant.create", ("admin-password", "p"), ("name", "x"));
        Assert.Equal(Commands.SignedCommandPreview(a), Commands.SignedCommandPreview(b));
    }

    [Fact]
    public void UnknownOperationIsRefused() =>
        Assert.Throws<KeyNotFoundException>(() => Commands.Cli(AdminAction.Create("nope")));

    [Fact]
    public async Task PendingActionPaneAndExecutionShareOneRecord()
    {
        var model = new AdminConsoleViewModel(Fixture.Registry(), Commands, Fixture.InfraOnly, Fixture.Enterprise);
        var action = AdminAction.Create("server.restart", ("node", "n1"));
        model.Pending = action;
        Assert.Equal("spot admin server.restart --node n1", model.EquivalentCommandText);

        var executor = new RecordingExecutor();
        await model.ExecutePendingAsync(executor, CancellationToken.None);
        Assert.Same(action, executor.Executed.Single());
    }

    [Fact]
    public async Task SignedOperationDoesNotRunUntilSigned()
    {
        var model = new AdminConsoleViewModel(Fixture.Registry(), Commands, Fixture.InfraOnly, Fixture.Enterprise);
        model.Pending = AdminAction.Create("fleet.rotate-keys", ("group", "eu"));
        var executor = new RecordingExecutor();
        await Assert.ThrowsAsync<InvalidOperationException>(() => model.ExecutePendingAsync(executor, CancellationToken.None));
        Assert.Empty(executor.Executed);

        await model.Signing!.ApproveAsync(new SoftwareTestSigner("a", [1]), CancellationToken.None);
        await model.Signing.ApproveAsync(new SoftwareTestSigner("b", [2]), CancellationToken.None);
        await model.ExecutePendingAsync(executor, CancellationToken.None);
        Assert.Single(executor.Executed);
    }

    [Fact]
    public async Task PlainHttpEndpointIsRefused()
    {
        var manager = new ConnectionManager(new MemoryCredentialStore());
        await Assert.ThrowsAsync<ArgumentException>(() => manager.AddAsync(
            new ConnectionProfile("p", "P", new Uri("http://mgmt.example.test/")), CancellationToken.None));
    }

    [Fact]
    public async Task ExecutingWithNothingPendingFails() =>
        await Assert.ThrowsAsync<InvalidOperationException>(() =>
            new AdminConsoleViewModel(Fixture.Registry(), Commands, Fixture.InfraOnly, Fixture.Enterprise)
                .ExecutePendingAsync(new RecordingExecutor(), CancellationToken.None));
}

public sealed class SigningTests
{
    private static readonly EquivalentCommand Commands = new(Fixture.Catalogue);

    private static SoftwareTestSigner Signer(string id) => new(id, Encoding.UTF8.GetBytes("test-key-" + id));

    [Fact]
    public async Task TwoPersonOperationWaitsForSecondApprover()
    {
        var session = new SigningSession(AdminAction.Create("fleet.rotate-keys", ("group", "g")), Commands);
        Assert.Equal(SigningState.AwaitingFirstApproval, session.State);

        await session.ApproveAsync(Signer("alice"), CancellationToken.None);
        Assert.Equal(SigningState.AwaitingSecondApprover, session.State);
        Assert.Equal("Waiting for second approver", session.StatusText);

        await Assert.ThrowsAsync<InvalidOperationException>(() => session.ApproveAsync(Signer("alice"), CancellationToken.None));

        await session.ApproveAsync(Signer("bob"), CancellationToken.None);
        Assert.Equal(SigningState.Complete, session.State);
        Assert.Equal(2, session.Approvals.Count);
        Assert.NotEqual(session.Approvals[0].Signature, session.Approvals[1].Signature);
    }

    [Fact]
    public async Task UnsignedOperationNeedsNoSignature()
    {
        var session = new SigningSession(AdminAction.Create("server.restart", ("node", "n")), Commands);
        Assert.Equal(SigningState.NotRequired, session.State);
        await Assert.ThrowsAsync<InvalidOperationException>(() => session.ApproveAsync(Signer("alice"), CancellationToken.None));
    }
}

public sealed class SettingsScreenTests
{
    private static SettingsScreenModel Model(string schemaJson = Fixture.SettingsSchemaJson) =>
        new(SettingsSchema.Parse(schemaJson), new Dictionary<string, SettingValue>
        {
            ["http.port"] = new("9090", InheritedFrom: "cluster"),
        });

    [Fact]
    public void RowsCarryBadgesAndInheritance()
    {
        var port = Model().Rows().Single(r => r.Descriptor.Key == "http.port");
        Assert.True(port.DiffersFromDefault);
        Assert.Equal("cluster", port.InheritedFrom);
        Assert.True(port.Descriptor.RestartNeeded);
        Assert.Equal(SettingRisk.Medium, port.Descriptor.Risk);
        Assert.Equal("info", Model().Rows().Single(r => r.Descriptor.Key == "log.level").Value);
    }

    [Fact]
    public void NewSchemaSettingAppearsWithNoUiCode()
    {
        var extended = Fixture.SettingsSchemaJson.Replace(
            "]}",
            ",{\"key\":\"cache.size\",\"type\":\"int\",\"default\":\"64\",\"description\":\"Cache MB\",\"risk\":\"high\"}]}",
            StringComparison.Ordinal);
        var row = Model(extended).Rows().Single(r => r.Descriptor.Key == "cache.size");
        Assert.Equal("64", row.Value);
        Assert.Equal(SettingRisk.High, row.Descriptor.Risk);
    }

    [Fact]
    public void SearchAndDifferingFiltersApply()
    {
        var model = Model();
        model.Search = "verbosity";
        Assert.Equal(["log.level"], model.Rows().Select(r => r.Descriptor.Key));
        model.Search = string.Empty;
        model.OnlyDifferingFromDefault = true;
        Assert.Equal(["http.port"], model.Rows().Select(r => r.Descriptor.Key));
    }

    [Fact]
    public void ServerValidationErrorsShowInline()
    {
        var model = Model();
        model.SetServerErrors(new Dictionary<string, string> { ["http.port"] = "must be below 65536" });
        Assert.Equal("must be below 65536", model.Rows().Single(r => r.Descriptor.Key == "http.port").Error);
        Assert.Null(model.Rows().Single(r => r.Descriptor.Key == "log.level").Error);
    }
}
