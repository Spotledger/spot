namespace Spotledger.AdminCenter;

/// <summary>
/// The two administrator roles. Neither implies the other (ADR-070 s10.4): an infrastructure
/// administrator sees no ERP tools and an ERP administrator sees no infrastructure tools.
/// </summary>
public enum AdminRole
{
    InfrastructureAdministrator,
    ErpAdministrator,
}

/// <summary>The signed-in principal as the console sees it: a name and the roles it holds.</summary>
public sealed record AdminPrincipal(string Name, IReadOnlySet<AdminRole> Roles)
{
    public bool Holds(AdminRole role) => Roles.Contains(role);
}

/// <summary>
/// Licence entitlements for the connected deployment (from the entitlements API of
/// <c>spot-license</c>): the edition name and the capability strings it grants.
/// </summary>
public sealed record Entitlements(string Edition, IReadOnlySet<string> Capabilities)
{
    public bool Grants(string capability) => Capabilities.Contains(capability);
}
