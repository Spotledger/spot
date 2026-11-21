namespace Spotledger.AdminCenter.Tenants;

/// <summary>
/// The tenant and fleet tools, registered once in the Admin Center's tool registry. Tenant tools are
/// for the ERP administrator; fleet tools for the infrastructure administrator and, being a licensed
/// feature, need the fleet capability.
/// </summary>
public static class TenantTools
{
    /// <summary>Tenants and companies.</summary>
    public const string TenantsId = "tenant.tenants";

    /// <summary>Apps.</summary>
    public const string AppsId = "tenant.apps";

    /// <summary>Migrations.</summary>
    public const string MigrationsId = "tenant.migrations";

    /// <summary>Desired state and drift.</summary>
    public const string DesiredStateId = "fleet.desired-state";

    /// <summary>Fleet health.</summary>
    public const string FleetHealthId = "fleet.health";

    /// <summary>The licence capability the fleet tools need.</summary>
    public const string FleetCapability = "fleet-management";

    /// <summary>Every entry, in navigation order.</summary>
    public static IReadOnlyList<ToolEntry> Entries { get; } =
    [
        new(TenantsId, "Tenants and companies", ToolCategory.Tenant, AdminRole.ErpAdministrator),
        new(AppsId, "Apps", ToolCategory.Tenant, AdminRole.ErpAdministrator),
        new(MigrationsId, "Migrations", ToolCategory.Tenant, AdminRole.ErpAdministrator),
        new(DesiredStateId, "Desired state and drift", ToolCategory.Fleet, AdminRole.InfrastructureAdministrator, FleetCapability),
        new(FleetHealthId, "Fleet health", ToolCategory.Fleet, AdminRole.InfrastructureAdministrator, FleetCapability),
    ];

    /// <summary>Adds the tools to <paramref name="registry"/>.</summary>
    public static void Register(ToolRegistry registry)
    {
        ArgumentNullException.ThrowIfNull(registry);
        foreach (var entry in Entries)
        {
            registry.Register(entry);
        }
    }
}
