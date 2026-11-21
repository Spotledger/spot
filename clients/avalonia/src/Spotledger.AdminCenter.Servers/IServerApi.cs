namespace Spotledger.AdminCenter.Servers;

/// <summary>
/// The typed management API for one connected server. Responses are typed records, never scraped
/// text. Instants are arguments of the callers; nothing here reads the clock.
/// </summary>
public interface IServerApi
{
    /// <summary>What the agent reports it can do.</summary>
    Task<ServerCapabilities> GetCapabilitiesAsync(CancellationToken cancellationToken);

    /// <summary>The overview.</summary>
    Task<HostOverview> GetOverviewAsync(CancellationToken cancellationToken);

    /// <summary>Services.</summary>
    Task<IReadOnlyList<ServiceInfo>> GetServicesAsync(CancellationToken cancellationToken);

    /// <summary>Packages and updates.</summary>
    Task<PackageReport> GetPackagesAsync(CancellationToken cancellationToken);

    /// <summary>Users.</summary>
    Task<IReadOnlyList<UserInfo>> GetUsersAsync(CancellationToken cancellationToken);

    /// <summary>Firewall rules.</summary>
    Task<IReadOnlyList<FirewallRule>> GetFirewallAsync(CancellationToken cancellationToken);

    /// <summary>Volumes and snapshots.</summary>
    Task<StorageReport> GetStorageAsync(CancellationToken cancellationToken);

    /// <summary>Network interfaces.</summary>
    Task<IReadOnlyList<NetworkInterfaceInfo>> GetNetworkAsync(CancellationToken cancellationToken);

    /// <summary>Certificates.</summary>
    Task<IReadOnlyList<CertificateInfo>> GetCertificatesAsync(CancellationToken cancellationToken);

    /// <summary>The module's plan for an action: nothing changes.</summary>
    Task<PlanPreview> PreviewAsync(AdminAction action, CancellationToken cancellationToken);

    /// <summary>
    /// Runs an action with the signatures collected for it and streams progress; the last item
    /// carries the result with its verify check. The server refuses an action whose signatures
    /// do not satisfy the operation's risk.
    /// </summary>
    IAsyncEnumerable<OperationProgress> RunAsync(AdminAction action, IReadOnlyList<Approval> approvals, CancellationToken cancellationToken);

    /// <summary>Confirms a change that has a revert timer, before it reverts itself.</summary>
    Task ConfirmAsync(RevertTicket ticket, CancellationToken cancellationToken);
}
