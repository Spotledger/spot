namespace Spotledger.AdminCenter.Deploy;

/// <summary>Registers the two wizards in the Admin Center's tool registry.</summary>
public static class DeployTools
{
    /// <summary>Id of the Add server wizard.</summary>
    public const string AddServerId = "deploy.add-server";

    /// <summary>Id of the Create cluster wizard.</summary>
    public const string CreateClusterId = "deploy.create-cluster";

    /// <summary>The entries, in navigation order. Both are infrastructure tools (no ERP administrator sees them).</summary>
    public static IReadOnlyList<ToolEntry> Entries { get; } =
    [
        new(AddServerId, "Add server", ToolCategory.Server, AdminRole.InfrastructureAdministrator),
        new(CreateClusterId, "Create cluster", ToolCategory.Fleet, AdminRole.InfrastructureAdministrator),
    ];

    /// <summary>Adds the wizards to <paramref name="registry"/>.</summary>
    public static void Register(ToolRegistry registry)
    {
        ArgumentNullException.ThrowIfNull(registry);
        foreach (var entry in Entries)
        {
            registry.Register(entry);
        }
    }
}
