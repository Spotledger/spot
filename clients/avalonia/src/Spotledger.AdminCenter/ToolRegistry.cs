namespace Spotledger.AdminCenter;

public enum ToolCategory
{
    Server,
    Tenant,
    Fleet,
}

/// <summary>
/// One navigation entry a tool registers. <see cref="RequiredRole"/> separates the
/// infrastructure and ERP trees; <see cref="RequiredCapability"/> is a licence edition
/// feature (null means every edition).
/// </summary>
public sealed record ToolEntry(
    string Id,
    string Title,
    ToolCategory Category,
    AdminRole RequiredRole,
    string? RequiredCapability = null);

/// <summary>A computed navigation entry: shown or disabled, with the reason as a tooltip.</summary>
public sealed record NavigationItem(ToolEntry Tool, bool Enabled, string? Tooltip);

/// <summary>
/// Tools (server, tenant, fleet) register here. Navigation is computed from the principal's
/// roles and the deployment's entitlements: a missing role hides the entry (the other role's
/// tree), a missing licence capability disables it with an explanatory tooltip, so nothing
/// fails at click time.
/// </summary>
public sealed class ToolRegistry
{
    private readonly List<ToolEntry> _tools = [];

    public IReadOnlyList<ToolEntry> Tools => _tools;

    public void Register(ToolEntry tool)
    {
        ArgumentNullException.ThrowIfNull(tool);
        if (_tools.Exists(existing => string.Equals(existing.Id, tool.Id, StringComparison.Ordinal)))
        {
            throw new InvalidOperationException($"A tool with id '{tool.Id}' is already registered.");
        }

        _tools.Add(tool);
    }

    public IReadOnlyList<NavigationItem> Navigation(AdminPrincipal principal, Entitlements entitlements)
    {
        ArgumentNullException.ThrowIfNull(principal);
        ArgumentNullException.ThrowIfNull(entitlements);
        var items = new List<NavigationItem>();
        foreach (var tool in _tools)
        {
            if (!principal.Holds(tool.RequiredRole))
            {
                continue;
            }

            if (tool.RequiredCapability is { } capability && !entitlements.Grants(capability))
            {
                items.Add(new NavigationItem(
                    tool,
                    Enabled: false,
                    $"Requires the '{capability}' capability, which the {entitlements.Edition} edition does not include."));
                continue;
            }

            items.Add(new NavigationItem(tool, Enabled: true, Tooltip: null));
        }

        return items;
    }
}
