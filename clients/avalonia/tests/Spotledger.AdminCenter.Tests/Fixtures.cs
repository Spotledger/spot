namespace Spotledger.AdminCenter.Tests;

internal sealed class MemoryCredentialStore : ICredentialStore
{
    public Dictionary<string, string> Items { get; } = new(StringComparer.Ordinal);

    public Task<string?> ReadAsync(string key, CancellationToken cancellationToken) =>
        Task.FromResult(Items.TryGetValue(key, out var value) ? value : null);

    public Task WriteAsync(string key, string value, CancellationToken cancellationToken)
    {
        Items[key] = value;
        return Task.CompletedTask;
    }

    public Task DeleteAsync(string key, CancellationToken cancellationToken)
    {
        Items.Remove(key);
        return Task.CompletedTask;
    }
}

internal sealed class FakeProbe(string fingerprint, HealthBadge health = HealthBadge.Healthy, bool fail = false) : IManagementProbe
{
    public Task<ProbeResult> ProbeAsync(ConnectionProfile profile, CancellationToken cancellationToken) =>
        fail ? throw new HttpRequestException("down") : Task.FromResult(new ProbeResult(fingerprint, health));
}

internal sealed class FakePrompt(bool answer) : IPinPrompt
{
    public int Asked { get; private set; }

    public Task<bool> TrustAsync(ConnectionProfile profile, string fingerprint, CancellationToken cancellationToken)
    {
        Asked++;
        return Task.FromResult(answer);
    }
}

internal sealed class RecordingExecutor : IAdminExecutor
{
    public List<AdminAction> Executed { get; } = [];

    public Task ExecuteAsync(AdminAction action, CancellationToken cancellationToken)
    {
        Executed.Add(action);
        return Task.CompletedTask;
    }
}

internal static class Fixture
{
    public static readonly AdminPrincipal InfraOnly = new("ines", new HashSet<AdminRole> { AdminRole.InfrastructureAdministrator });

    public static readonly AdminPrincipal ErpOnly = new("erin", new HashSet<AdminRole> { AdminRole.ErpAdministrator });

    public static readonly Entitlements Community = new("Community", new HashSet<string>());

    public static readonly Entitlements Enterprise = new("Enterprise", new HashSet<string> { "fleet" });

    public static readonly OperationSpec[] Catalogue =
    [
        new("server.restart", [new ArgumentSpec("node")]),
        new("tenant.create", [new ArgumentSpec("name"), new ArgumentSpec("admin-password", IsSecret: true)]),
        new("fleet.rotate-keys", [new ArgumentSpec("group")], RequiredApprovals: 2),
    ];

    public static ToolRegistry Registry()
    {
        var registry = new ToolRegistry();
        registry.Register(new ToolEntry("server.nodes", "Nodes", ToolCategory.Server, AdminRole.InfrastructureAdministrator));
        registry.Register(new ToolEntry("fleet.rollout", "Rollouts", ToolCategory.Fleet, AdminRole.InfrastructureAdministrator, "fleet"));
        registry.Register(new ToolEntry("tenant.users", "Users", ToolCategory.Tenant, AdminRole.ErpAdministrator));
        return registry;
    }

    public const string SettingsSchemaJson = """
        {"settings":[
          {"key":"http.port","type":"int","default":"8080","description":"Listen port","restart_needed":true,"risk":"medium"},
          {"key":"log.level","type":"string","default":"info","description":"Log verbosity","risk":"low"}
        ]}
        """;
}
