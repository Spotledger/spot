namespace Spotledger.AdminCenter.Servers;

/// <summary>Packages and updates: installed, pending, apply with the reboot warning.</summary>
public sealed class PackagesScreen : ServerScreen
{
    private PackageReport _report = new([], [], false);

    /// <summary>Creates the screen.</summary>
    public PackagesScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.PackagesId;

    /// <inheritdoc />
    public override string Title => "Packages and updates";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Installed", "Pending", "ApplyUpdates"];

    /// <summary>Installed packages.</summary>
    public IReadOnlyList<PackageInfo> Installed => _report.Installed;

    /// <summary>Updates waiting to be applied.</summary>
    public IReadOnlyList<PackageInfo> Pending => _report.Pending;

    /// <summary>True when the server already needs a reboot.</summary>
    public bool RebootRequired => _report.RebootRequired;

    /// <summary>A warning when applying will, or already does, need a reboot; null otherwise.</summary>
    public string? RebootWarning => RebootRequired
        ? "This server already needs a reboot. Reboot it before applying more updates."
        : Pending.Any(p => p.NeedsReboot)
            ? $"{Pending.Count(p => p.NeedsReboot)} of the pending updates need a reboot to finish. Plan a time for it."
            : null;

    /// <summary>True when there is something to apply and the server can apply it.</summary>
    public bool CanApply => Pending.Count > 0 && Capabilities.SupportsAny(Ops.UpdateApply, Ops.UpdateInstall);

    /// <summary>Previews applying all pending updates (Linux: apply, Windows: install, from the capabilities).</summary>
    public Task<bool> ApplyUpdatesAsync(CancellationToken cancellationToken = default)
    {
        var operation = Capabilities.Supports(Ops.UpdateApply) ? Ops.UpdateApply : Ops.UpdateInstall;
        return RequestAsync(AdminAction.Create(operation, ("scope", "all")), cancellationToken);
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _report = await Api.GetPackagesAsync(cancellationToken);
        OnPropertyChanged(nameof(Installed));
        OnPropertyChanged(nameof(Pending));
        OnPropertyChanged(nameof(RebootRequired));
        OnPropertyChanged(nameof(RebootWarning));
        OnPropertyChanged(nameof(CanApply));
    }
}

/// <summary>Users: list, create, lock and unlock (or disable on Windows).</summary>
public sealed class UsersScreen : ServerScreen
{
    private IReadOnlyList<UserInfo> _users = [];

    /// <summary>Creates the screen.</summary>
    public UsersScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.UsersId;

    /// <inheritdoc />
    public override string Title => "Users";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Users", "NewUser", "Lock", "Unlock"];

    /// <summary>The accounts.</summary>
    public IReadOnlyList<UserInfo> Users => _users;

    /// <summary>True when accounts can be locked or disabled on this server.</summary>
    public bool CanLock => Capabilities.SupportsAny(Ops.UserLock, Ops.UserDisable);

    /// <summary>Previews creating an account. No password is collected here.</summary>
    public Task<bool> CreateAsync(string name, CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(name))
        {
            Error = "Enter a user name.";
            return Task.FromResult(false);
        }

        if (_users.Any(u => string.Equals(u.Name, name.Trim(), StringComparison.OrdinalIgnoreCase)))
        {
            Error = $"{name.Trim()} already exists.";
            return Task.FromResult(false);
        }

        return RequestAsync(AdminAction.Create(Ops.UserCreate, ("name", name.Trim())), cancellationToken);
    }

    /// <summary>Previews locking an account (Linux) or disabling it (Windows).</summary>
    public Task<bool> LockAsync(string name, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Capabilities.Supports(Ops.UserLock) ? Ops.UserLock : Ops.UserDisable, ("name", name)), cancellationToken);

    /// <summary>Previews unlocking an account (Linux only).</summary>
    public Task<bool> UnlockAsync(string name, CancellationToken cancellationToken = default) =>
        RequestAsync(AdminAction.Create(Ops.UserUnlock, ("name", name)), cancellationToken);

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _users = await Api.GetUsersAsync(cancellationToken);
        OnPropertyChanged(nameof(Users));
    }
}

/// <summary>
/// Firewall: rules list, add, remove. A change that could lock the administrator out carries the
/// module's warning in its preview; the operation runner will not run until it is acknowledged.
/// </summary>
public sealed class FirewallScreen : ServerScreen
{
    private IReadOnlyList<FirewallRule> _rules = [];

    /// <summary>Creates the screen.</summary>
    public FirewallScreen(ServerContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => ServerTools.FirewallId;

    /// <inheritdoc />
    public override string Title => "Firewall";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Rules", "AddRule", "RemoveRule"];

    /// <summary>The rules.</summary>
    public IReadOnlyList<FirewallRule> Rules => _rules;

    /// <summary>Previews allowing a port.</summary>
    public Task<bool> AddAsync(ushort port, string protocol, string description, CancellationToken cancellationToken = default)
    {
        if (port == 0)
        {
            Error = "Enter a port from 1 to 65535.";
            return Task.FromResult(false);
        }

        var text = port.ToString(System.Globalization.CultureInfo.InvariantCulture);
        var action = Capabilities.Supports(Ops.FirewallOpen)
            ? AdminAction.Create(Ops.FirewallOpen, ("port", text), ("protocol", protocol))
            : AdminAction.Create(Ops.FirewallAddRule, ("name", description), ("port", text), ("protocol", protocol));
        return RequestAsync(action, cancellationToken);
    }

    /// <summary>Previews removing a rule.</summary>
    public Task<bool> RemoveAsync(FirewallRule rule, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(rule);
        var text = rule.Port.ToString(System.Globalization.CultureInfo.InvariantCulture);
        var action = Capabilities.Supports(Ops.FirewallClose)
            ? AdminAction.Create(Ops.FirewallClose, ("port", text), ("protocol", rule.Protocol))
            : AdminAction.Create(Ops.FirewallRemoveRule, ("name", rule.Id));
        return RequestAsync(action, cancellationToken);
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _rules = await Api.GetFirewallAsync(cancellationToken);
        OnPropertyChanged(nameof(Rules));
    }
}
