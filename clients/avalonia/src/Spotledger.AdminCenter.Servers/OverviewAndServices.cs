namespace Spotledger.AdminCenter.Servers;

/// <summary>Host overview: OS, uptime, load, disks and the pending-reboot flag. Read only.</summary>
public sealed class OverviewScreen : ServerScreen
{
    private HostOverview? _overview;

    /// <summary>Creates the screen.</summary>
    public OverviewScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.OverviewId;

    /// <inheritdoc />
    public override string Title => "Overview";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Refresh"];

    /// <summary>The data, once loaded.</summary>
    public HostOverview? Overview
    {
        get => _overview;
        private set
        {
            if (SetField(ref _overview, value))
            {
                OnPropertyChanged(nameof(UptimeText));
                OnPropertyChanged(nameof(LoadText));
                OnPropertyChanged(nameof(RebootBanner));
                OnPropertyChanged(nameof(FullestDisk));
            }
        }
    }

    /// <summary>Uptime as "3 days 4 hours 5 minutes".</summary>
    public string? UptimeText => Overview is null ? null : Duration(Overview.UptimeSeconds);

    /// <summary>The three load averages, to two decimals, from integer thousandths.</summary>
    public string? LoadText => Overview is null
        ? null
        : $"{Milli(Overview.LoadMilli1)} / {Milli(Overview.LoadMilli5)} / {Milli(Overview.LoadMilli15)}";

    /// <summary>A banner when the server needs a reboot; null otherwise.</summary>
    public string? RebootBanner => Overview is { PendingReboot: true } ? "This server needs a reboot to finish applying changes." : null;

    /// <summary>The disk with the least free space, in whole percent used.</summary>
    public string? FullestDisk
    {
        get
        {
            var disk = Overview?.Disks.Where(d => d.TotalGigabytes > 0).OrderBy(d => d.FreeGigabytes * 100 / d.TotalGigabytes).FirstOrDefault();
            return disk is null ? null : $"{disk.Mount}: {100 - (disk.FreeGigabytes * 100 / disk.TotalGigabytes)}% used";
        }
    }

    internal static string Duration(ulong seconds)
    {
        var days = seconds / 86_400;
        var hours = seconds % 86_400 / 3_600;
        var minutes = seconds % 3_600 / 60;
        return $"{days} days {hours} hours {minutes} minutes";
    }

    private static string Milli(uint milli) => $"{milli / 1000}.{milli % 1000 / 10:00}";

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken) =>
        Overview = await Api.GetOverviewAsync(cancellationToken);
}

/// <summary>Which services to list.</summary>
public enum ServiceFilter
{
    /// <summary>All services.</summary>
    All,

    /// <summary>Running ones.</summary>
    Running,

    /// <summary>Stopped ones.</summary>
    Stopped,

    /// <summary>Failed ones.</summary>
    Failed,
}

/// <summary>Services: list, filter, start, stop, restart, enable or disable at boot.</summary>
public sealed class ServicesScreen : ServerScreen
{
    private IReadOnlyList<ServiceInfo> _all = [];
    private string _search = string.Empty;
    private ServiceFilter _filter;

    /// <summary>Creates the screen.</summary>
    public ServicesScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.ServicesId;

    /// <inheritdoc />
    public override string Title => "Services";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Search", "StateFilter", "Services", "Start", "Stop", "Restart", "Enable"];

    /// <summary>The text to match in names and descriptions.</summary>
    public string Search
    {
        get => _search;
        set
        {
            if (SetField(ref _search, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>The state filter.</summary>
    public ServiceFilter Filter
    {
        get => _filter;
        set
        {
            if (SetField(ref _filter, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>The services that pass the search and the filter.</summary>
    public IReadOnlyList<ServiceInfo> Visible => [.. _all.Where(Matches)];

    private bool Matches(ServiceInfo service) =>
        (Filter == ServiceFilter.All || (int)service.State == (int)Filter - 1)
        && (string.IsNullOrWhiteSpace(Search)
            || service.Name.Contains(Search, StringComparison.OrdinalIgnoreCase)
            || service.Description.Contains(Search, StringComparison.OrdinalIgnoreCase));

    /// <summary>True when starting makes sense for <paramref name="service"/>.</summary>
    public bool CanStart(ServiceInfo service) => service.State != ServiceState.Running && Capabilities.Supports(Ops.ServiceStart);

    /// <summary>True when stopping makes sense.</summary>
    public bool CanStop(ServiceInfo service) => service.State == ServiceState.Running && Capabilities.Supports(Ops.ServiceStop);

    /// <summary>True when restarting makes sense.</summary>
    public bool CanRestart(ServiceInfo service) => service.State == ServiceState.Running && Capabilities.Supports(Ops.ServiceRestart);

    /// <summary>True when the boot setting can be changed on this server.</summary>
    public bool CanSetBoot => Capabilities.SupportsAny(Ops.ServiceEnable, Ops.ServiceSetStartType);

    /// <summary>Previews starting a service.</summary>
    public Task<bool> StartAsync(string name, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.ServiceStart, ("name", name)), cancellationToken);

    /// <summary>Previews stopping a service.</summary>
    public Task<bool> StopAsync(string name, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.ServiceStop, ("name", name)), cancellationToken);

    /// <summary>Previews restarting a service.</summary>
    public Task<bool> RestartAsync(string name, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.ServiceRestart, ("name", name)), cancellationToken);

    /// <summary>
    /// Previews turning boot start on or off. The operation comes from the agent's capabilities:
    /// enable and disable where it has them, otherwise the start-type operation.
    /// </summary>
    public Task<bool> SetEnabledAsync(string name, bool enabled, CancellationToken cancellationToken = default)
    {
        if (Capabilities.Supports(Ops.ServiceEnable) && Capabilities.Supports(Ops.ServiceDisable))
        {
            return RequestAsync(AdminAction.Create(enabled ? Ops.ServiceEnable : Ops.ServiceDisable, ("name", name)), cancellationToken);
        }

        return RequestAsync(
            AdminAction.Create(Ops.ServiceSetStartType, ("name", name), ("start_type", enabled ? "automatic" : "disabled")),
            cancellationToken);
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _all = await Api.GetServicesAsync(cancellationToken);
        OnPropertyChanged(nameof(Visible));
    }
}
