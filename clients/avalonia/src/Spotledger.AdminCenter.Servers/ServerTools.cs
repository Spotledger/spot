namespace Spotledger.AdminCenter.Servers;

/// <summary>
/// The server tools: registered once in the Admin Center's tool registry, and chosen per server
/// from the capabilities its agent reports, so a Windows server shows the Windows module set and
/// a Linux server the Linux set without any check on the OS name.
/// </summary>
public static class ServerTools
{
    /// <summary>Overview.</summary>
    public const string OverviewId = "server.overview";

    /// <summary>Services.</summary>
    public const string ServicesId = "server.services";

    /// <summary>Packages and updates.</summary>
    public const string PackagesId = "server.packages";

    /// <summary>Users.</summary>
    public const string UsersId = "server.users";

    /// <summary>Firewall.</summary>
    public const string FirewallId = "server.firewall";

    /// <summary>Storage and snapshots.</summary>
    public const string StorageId = "server.storage";

    /// <summary>Network.</summary>
    public const string NetworkId = "server.network";

    /// <summary>Certificates.</summary>
    public const string CertificatesId = "server.certificates";

    /// <summary>Every entry, in navigation order. All are for infrastructure administrators.</summary>
    public static IReadOnlyList<ToolEntry> Entries { get; } =
    [
        Entry(OverviewId, "Overview"),
        Entry(ServicesId, "Services"),
        Entry(PackagesId, "Packages and updates"),
        Entry(UsersId, "Users"),
        Entry(FirewallId, "Firewall"),
        Entry(StorageId, "Storage and snapshots"),
        Entry(NetworkId, "Network"),
        Entry(CertificatesId, "Certificates"),
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

    /// <summary>The ids of the tools a server with these capabilities offers.</summary>
    public static IReadOnlyList<string> AvailableFor(ServerCapabilities capabilities)
    {
        ArgumentNullException.ThrowIfNull(capabilities);
        var ids = new List<string> { OverviewId };
        Add(ids, ServicesId, capabilities.SupportsAny(Ops.ServiceList, Ops.ServiceStatus));
        Add(ids, PackagesId, capabilities.SupportsAny(Ops.PackageList, Ops.UpdateCheck, Ops.UpdatePending));
        Add(ids, UsersId, capabilities.Supports(Ops.UserList));
        Add(ids, FirewallId, capabilities.Supports(Ops.FirewallList));
        Add(ids, StorageId, capabilities.SupportsAny(Ops.StorageFacts, Ops.SnapshotCreate));
        Add(ids, NetworkId, capabilities.Supports(Ops.NetworkSet));
        Add(ids, CertificatesId, capabilities.Supports(Ops.CertInstall));
        return ids;
    }

    /// <summary>Builds the screens a server offers, in navigation order.</summary>
    public static IReadOnlyList<ServerScreen> CreateScreens(ServerContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        return
        [
            .. AvailableFor(context.Capabilities).Select<string, ServerScreen>(id => id switch
            {
                OverviewId => new OverviewScreen(context),
                ServicesId => new ServicesScreen(context),
                PackagesId => new PackagesScreen(context),
                UsersId => new UsersScreen(context),
                FirewallId => new FirewallScreen(context),
                StorageId => new StorageScreen(context),
                NetworkId => new NetworkScreen(context),
                _ => new CertificatesScreen(context),
            }),
        ];
    }

    private static ToolEntry Entry(string id, string title) => new(id, title, ToolCategory.Server, AdminRole.InfrastructureAdministrator);

    private static void Add(List<string> ids, string id, bool supported)
    {
        if (supported)
        {
            ids.Add(id);
        }
    }
}
