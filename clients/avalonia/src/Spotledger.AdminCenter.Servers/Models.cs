namespace Spotledger.AdminCenter.Servers;

/// <summary>The operating-system family an agent reports.</summary>
public enum OsFamily
{
    /// <summary>A Linux server.</summary>
    Linux,

    /// <summary>A Windows server.</summary>
    Windows,
}

/// <summary>
/// What the agent on a server reports it can do: the operating system and the operation ids it
/// has registered. Screens and buttons follow this, never a string check on the OS name.
/// </summary>
public sealed record ServerCapabilities(OsFamily Os, IReadOnlySet<string> Operations)
{
    /// <summary>True when the agent has registered <paramref name="operationId"/>.</summary>
    public bool Supports(string operationId) => Operations.Contains(operationId);

    /// <summary>True when the agent has registered any of the operations.</summary>
    public bool SupportsAny(params string[] operationIds) => operationIds.Any(Operations.Contains);
}

/// <summary>The operation ids the screens use. They are the catalogue's (<c>os.&lt;group&gt;.&lt;action&gt;</c>).</summary>
public static class Ops
{
    /// <summary>Lists services.</summary>
    public const string ServiceList = "os.service.list";

    /// <summary>Service status (Linux).</summary>
    public const string ServiceStatus = "os.service.status";

    /// <summary>Starts a service.</summary>
    public const string ServiceStart = "os.service.start";

    /// <summary>Stops a service.</summary>
    public const string ServiceStop = "os.service.stop";

    /// <summary>Restarts a service.</summary>
    public const string ServiceRestart = "os.service.restart";

    /// <summary>Enables a service at boot (Linux).</summary>
    public const string ServiceEnable = "os.service.enable";

    /// <summary>Disables a service at boot (Linux).</summary>
    public const string ServiceDisable = "os.service.disable";

    /// <summary>Sets a service's start type (Windows).</summary>
    public const string ServiceSetStartType = "os.service.set_start_type";

    /// <summary>Lists installed packages (Linux).</summary>
    public const string PackageList = "os.package.list";

    /// <summary>Checks for updates (Linux).</summary>
    public const string UpdateCheck = "os.update.check";

    /// <summary>Applies updates (Linux).</summary>
    public const string UpdateApply = "os.update.apply";

    /// <summary>Lists pending updates (Windows).</summary>
    public const string UpdatePending = "os.update.pending";

    /// <summary>Installs updates (Windows).</summary>
    public const string UpdateInstall = "os.update.install";

    /// <summary>Lists users.</summary>
    public const string UserList = "os.user.list";

    /// <summary>Creates a user.</summary>
    public const string UserCreate = "os.user.create";

    /// <summary>Locks a user (Linux).</summary>
    public const string UserLock = "os.user.lock";

    /// <summary>Unlocks a user (Linux).</summary>
    public const string UserUnlock = "os.user.unlock";

    /// <summary>Disables a user (Windows).</summary>
    public const string UserDisable = "os.user.disable";

    /// <summary>Lists firewall rules.</summary>
    public const string FirewallList = "os.firewall.list";

    /// <summary>Opens a port (Linux).</summary>
    public const string FirewallOpen = "os.firewall.open";

    /// <summary>Closes a port (Linux).</summary>
    public const string FirewallClose = "os.firewall.close";

    /// <summary>Adds a rule (Windows).</summary>
    public const string FirewallAddRule = "os.firewall.add_rule";

    /// <summary>Removes a rule (Windows).</summary>
    public const string FirewallRemoveRule = "os.firewall.remove_rule";

    /// <summary>Reads storage facts.</summary>
    public const string StorageFacts = "os.storage.facts";

    /// <summary>Creates a snapshot.</summary>
    public const string SnapshotCreate = "os.snapshot.create";

    /// <summary>Restores a snapshot.</summary>
    public const string SnapshotRestore = "os.snapshot.restore";

    /// <summary>Changes network addressing, with a revert timer.</summary>
    public const string NetworkSet = "os.network.set";

    /// <summary>Installs a certificate.</summary>
    public const string CertInstall = "os.cert.install";
}

/// <summary>A disk's use.</summary>
public sealed record DiskUse(string Mount, ulong TotalGigabytes, ulong FreeGigabytes);

/// <summary>The overview screen's data. Load is in thousandths (1500 means 1.5); time is whole seconds.</summary>
public sealed record HostOverview(
    string OsName,
    string Hostname,
    ulong UptimeSeconds,
    uint LoadMilli1,
    uint LoadMilli5,
    uint LoadMilli15,
    IReadOnlyList<DiskUse> Disks,
    bool PendingReboot);

/// <summary>A service's run state.</summary>
public enum ServiceState
{
    /// <summary>Running.</summary>
    Running,

    /// <summary>Stopped.</summary>
    Stopped,

    /// <summary>Failed.</summary>
    Failed,
}

/// <summary>One service.</summary>
public sealed record ServiceInfo(string Name, string Description, ServiceState State, bool EnabledAtBoot);

/// <summary>One package, installed or with an update available.</summary>
public sealed record PackageInfo(string Name, string Version, string? AvailableVersion, bool NeedsReboot);

/// <summary>Packages and updates.</summary>
public sealed record PackageReport(IReadOnlyList<PackageInfo> Installed, IReadOnlyList<PackageInfo> Pending, bool RebootRequired);

/// <summary>One account.</summary>
public sealed record UserInfo(string Name, bool Locked, IReadOnlyList<string> Groups, bool IsAdministrator);

/// <summary>One firewall rule.</summary>
public sealed record FirewallRule(string Id, string Description, ushort Port, string Protocol, bool Allow);

/// <summary>A volume.</summary>
public sealed record VolumeInfo(string Name, string Mount, ulong TotalGigabytes, ulong UsedGigabytes);

/// <summary>A snapshot of a volume.</summary>
public sealed record SnapshotInfo(string Id, string Volume, string Label, long CreatedUnixSeconds);

/// <summary>Storage and snapshots.</summary>
public sealed record StorageReport(IReadOnlyList<VolumeInfo> Volumes, IReadOnlyList<SnapshotInfo> Snapshots);

/// <summary>A network interface.</summary>
public sealed record NetworkInterfaceInfo(string Name, bool Dhcp, IReadOnlyList<string> Addresses, string? Gateway);

/// <summary>One certificate with its expiry.</summary>
public sealed record CertificateInfo(string Subject, string Store, long NotAfterUnixSeconds);

/// <summary>How risky a change is.</summary>
public enum PreviewRisk
{
    /// <summary>Safe.</summary>
    Low,

    /// <summary>Visible to users.</summary>
    Medium,

    /// <summary>Can interrupt service.</summary>
    High,

    /// <summary>Can lose data or lock everyone out.</summary>
    Critical,
}

/// <summary>One value the operation would change.</summary>
public sealed record PreviewChange(string Field, string? Current, string? Desired);

/// <summary>
/// What a module's plan says an operation would do: what it adds, changes and removes, how risky
/// it is, whether it can be undone, and the warnings a person must read (a lock-out guard, a
/// reboot notice). Empty means the desired state already holds.
/// </summary>
public sealed record PlanPreview(
    string OperationId,
    string Summary,
    PreviewRisk Risk,
    IReadOnlyList<string> Adds,
    IReadOnlyList<PreviewChange> Changes,
    IReadOnlyList<string> Removes,
    bool Reversible,
    IReadOnlyList<string> Warnings,
    int? RevertWindowSeconds = null)
{
    /// <summary>True when applying would change nothing.</summary>
    public bool IsEmpty => Adds.Count == 0 && Changes.Count == 0 && Removes.Count == 0;
}

/// <summary>The result of re-reading the state after an apply.</summary>
public sealed record VerifyCheck(bool Ok, string Detail);

/// <summary>A change that reverts itself unless confirmed before its deadline.</summary>
public sealed record RevertTicket(string Id, long DeadlineUnixSeconds);

/// <summary>How an operation ended.</summary>
public sealed record OperationResult(bool Succeeded, string Detail, VerifyCheck? Verify, RevertTicket? Revert);

/// <summary>One streamed update from a running operation. The last one carries the result.</summary>
public sealed record OperationProgress(string Message, OperationResult? Final = null);

/// <summary>A refusal from the management API in its own words.</summary>
public sealed class ServerApiException : Exception
{
    /// <summary>Creates the exception.</summary>
    public ServerApiException(string message)
        : base(message)
    {
    }

    /// <summary>Creates the exception.</summary>
    public ServerApiException()
        : base("The management API refused the request.")
    {
    }

    /// <summary>Creates the exception.</summary>
    public ServerApiException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}
