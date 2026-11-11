namespace Spotledger.AdminCenter.Servers;

/// <summary>Storage and snapshots: volumes with use, snapshots, create and restore.</summary>
public sealed class StorageScreen : ServerScreen
{
    private StorageReport _report = new([], []);

    /// <summary>Creates the screen.</summary>
    public StorageScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.StorageId;

    /// <inheritdoc />
    public override string Title => "Storage and snapshots";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Volumes", "Snapshots", "CreateSnapshot", "RestoreSnapshot"];

    /// <summary>The volumes.</summary>
    public IReadOnlyList<VolumeInfo> Volumes => _report.Volumes;

    /// <summary>The snapshots, newest first.</summary>
    public IReadOnlyList<SnapshotInfo> Snapshots => [.. _report.Snapshots.OrderByDescending(s => s.CreatedUnixSeconds)];

    /// <summary>Whole percent used for a volume.</summary>
    public static int PercentUsed(VolumeInfo volume)
    {
        ArgumentNullException.ThrowIfNull(volume);
        return volume.TotalGigabytes == 0 ? 0 : (int)(volume.UsedGigabytes * 100 / volume.TotalGigabytes);
    }

    /// <summary>Previews taking a snapshot of a volume.</summary>
    public Task<bool> CreateSnapshotAsync(string volume, string label, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.SnapshotCreate, ("volume", volume), ("label", label)), cancellationToken);

    /// <summary>Previews restoring a snapshot. The preview says what is lost.</summary>
    public Task<bool> RestoreSnapshotAsync(string snapshotId, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.SnapshotRestore, ("snapshot", snapshotId)), cancellationToken);

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _report = await Api.GetStorageAsync(cancellationToken);
        OnPropertyChanged(nameof(Volumes));
        OnPropertyChanged(nameof(Snapshots));
    }
}

/// <summary>
/// Network: interfaces and addressing. A change applies with a revert timer; the operation runner
/// shows the countdown and the person confirms the server is still reachable.
/// </summary>
public sealed class NetworkScreen : ServerScreen
{
    private IReadOnlyList<NetworkInterfaceInfo> _interfaces = [];

    /// <summary>Creates the screen.</summary>
    public NetworkScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.NetworkId;

    /// <inheritdoc />
    public override string Title => "Network";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Interfaces", "Address", "Gateway", "Apply", "Confirm"];

    /// <summary>The interfaces.</summary>
    public IReadOnlyList<NetworkInterfaceInfo> Interfaces => _interfaces;

    /// <summary>Previews switching an interface to DHCP.</summary>
    public Task<bool> UseDhcpAsync(string name, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.NetworkSet, ("interface", name), ("mode", "dhcp")), cancellationToken);

    /// <summary>Previews giving an interface a fixed address and gateway.</summary>
    public Task<bool> UseStaticAsync(string name, string address, string gateway, CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(address) || string.IsNullOrWhiteSpace(gateway))
        {
            Error = "A fixed address needs an address and a gateway.";
            return Task.FromResult(false);
        }

        return RequestAsync(
            AdminAction.Create(Ops.NetworkSet, ("interface", name), ("mode", "static"), ("address", address.Trim()), ("gateway", gateway.Trim())),
            cancellationToken);
    }

    /// <summary>Confirms a change that is counting down, if the window is still open.</summary>
    public async Task<bool> ConfirmAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (Operation is not { Phase: OperationPhase.AwaitingConfirmation } operation)
        {
            return false;
        }

        var ok = await operation.ConfirmAsync(nowUnixSeconds, cancellationToken);
        if (ok)
        {
            await LoadAsync(nowUnixSeconds, cancellationToken);
        }
        else
        {
            Error = operation.Error;
        }

        return ok;
    }

    /// <summary>Advances the countdown of a pending confirmation.</summary>
    public void Tick(long nowUnixSeconds) => Operation?.Tick(nowUnixSeconds);

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _interfaces = await Api.GetNetworkAsync(cancellationToken);
        OnPropertyChanged(nameof(Interfaces));
    }
}

/// <summary>How soon a certificate expires.</summary>
public enum ExpiryLevel
{
    /// <summary>More than 30 days left.</summary>
    Fine,

    /// <summary>30 days or fewer.</summary>
    Soon,

    /// <summary>Already expired.</summary>
    Expired,
}

/// <summary>One certificate row with whole days left, measured from the caller's clock.</summary>
public sealed record CertificateRow(CertificateInfo Certificate, long DaysLeft, ExpiryLevel Level);

/// <summary>Certificates: an expiry list (soonest first) and install.</summary>
public sealed class CertificatesScreen : ServerScreen
{
    private const long SecondsPerDay = 86_400;
    private IReadOnlyList<CertificateInfo> _certificates = [];
    private long _now;

    /// <summary>Creates the screen.</summary>
    public CertificatesScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.CertificatesId;

    /// <inheritdoc />
    public override string Title => "Certificates";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Certificates", "Install"];

    /// <summary>The certificates, soonest expiry first, with days left.</summary>
    public IReadOnlyList<CertificateRow> Rows =>
    [
        .. _certificates.OrderBy(c => c.NotAfterUnixSeconds).Select(c =>
        {
            var left = FloorDays(c.NotAfterUnixSeconds - _now);
            return new CertificateRow(c, left, left < 0 ? ExpiryLevel.Expired : left <= 30 ? ExpiryLevel.Soon : ExpiryLevel.Fine);
        }),
    ];

    private static long FloorDays(long seconds) => seconds >= 0 ? seconds / SecondsPerDay : -((-seconds + SecondsPerDay - 1) / SecondsPerDay);

    /// <summary>Previews installing a certificate from PEM text (a public certificate, never a key).</summary>
    public Task<bool> InstallAsync(string name, string pem, CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(name) || !pem.Contains("BEGIN CERTIFICATE", StringComparison.Ordinal))
        {
            Error = "Give the certificate a name and paste a PEM certificate.";
            return Task.FromResult(false);
        }

        if (pem.Contains("PRIVATE KEY", StringComparison.Ordinal))
        {
            Error = "That text contains a private key. Paste the certificate only.";
            return Task.FromResult(false);
        }

        return RequestAsync(AdminAction.Create(Ops.CertInstall, ("name", name.Trim()), ("pem", pem)), cancellationToken);
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _now = nowUnixSeconds;
        _certificates = await Api.GetCertificatesAsync(cancellationToken);
        OnPropertyChanged(nameof(Rows));
    }
}
