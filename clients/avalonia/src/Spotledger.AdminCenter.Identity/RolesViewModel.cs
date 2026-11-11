using System.Windows.Input;
using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// Roles and duties: reference packs next to custom copies, the upgrade diff viewer and the duty
/// list. A reference role is never edited; a site copies it and edits the copy.
/// </summary>
public sealed class RolesViewModel : IdentityViewModel
{
    private IReadOnlyList<RoleSummary> _roles = [];
    private IReadOnlyList<DutyInfo> _duties = [];
    private UpgradeDiff? _diff;

    /// <summary>Creates the screen.</summary>
    public RolesViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
        RefreshCommand = new AsyncRelayCommand(_ => RefreshAsync());
    }

    /// <summary>Reloads roles and duties.</summary>
    public ICommand RefreshCommand { get; }

    /// <summary>Reference pack roles.</summary>
    public IReadOnlyList<RoleSummary> ReferenceRoles => _roles.Where(r => r.Kind == RoleKind.Reference).ToList();

    /// <summary>Site-owned copies.</summary>
    public IReadOnlyList<RoleSummary> CustomRoles => _roles.Where(r => r.Kind == RoleKind.Custom).ToList();

    /// <summary>The duty list.</summary>
    public IReadOnlyList<DutyInfo> Duties
    {
        get => _duties;
        private set => SetField(ref _duties, value);
    }

    /// <summary>The upgrade diff being viewed, if any.</summary>
    public UpgradeDiff? Diff
    {
        get => _diff;
        private set => SetField(ref _diff, value);
    }

    /// <summary>Loads roles and duties.</summary>
    public async Task RefreshAsync()
    {
        var roles = await CallAsync(() => Gateway.ListRolesAsync());
        if (roles is null)
        {
            return;
        }

        _roles = roles;
        OnPropertyChanged(nameof(ReferenceRoles));
        OnPropertyChanged(nameof(CustomRoles));
        var duties = await CallAsync(() => Gateway.ListDutiesAsync());
        if (duties is not null)
        {
            Duties = duties;
        }
    }

    /// <summary>Copies a reference role into a custom role. A custom role cannot be copied this way.</summary>
    public async Task<bool> CopyAsync(string roleId, string newName)
    {
        var role = _roles.FirstOrDefault(r => r.Id == roleId);
        if (role is null || role.Kind != RoleKind.Reference)
        {
            Error = "Only a reference role can be copied here.";
            return false;
        }

        if (string.IsNullOrWhiteSpace(newName))
        {
            Error = "Give the copy a name.";
            return false;
        }

        var copy = await CallAsync(() => Gateway.CopyRoleAsync(roleId, newName.Trim()));
        if (copy is null)
        {
            return false;
        }

        Status = $"Copied to {copy.Name}.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Shows what the newest pack version changes for a role.</summary>
    public async Task<bool> ViewUpgradeAsync(string roleId)
    {
        var role = _roles.FirstOrDefault(r => r.Id == roleId);
        if (role is null || !role.UpgradeAvailable)
        {
            Error = "This role has no upgrade available.";
            return false;
        }

        var diff = await CallAsync(() => Gateway.GetUpgradeDiffAsync(roleId));
        Diff = diff;
        return diff is not null;
    }

    /// <summary>Applies the upgrade that is being viewed, after the person confirms it. A diff must be open first.</summary>
    public async Task<bool> ApplyUpgradeAsync()
    {
        if (Diff is not { } diff)
        {
            Error = "Look at the changes before upgrading.";
            return false;
        }

        var removed = diff.Changes.Count(c => c.Kind == DutyChangeKind.Removed);
        var confirmed = await Shell.ConfirmAsync(
            "Upgrade role",
            $"{diff.Changes.Count} duty changes ({removed} removed) from {diff.FromVersion} to {diff.ToVersion}.",
            "Upgrade");
        if (!confirmed)
        {
            return false;
        }

        var role = await CallAsync(() => Gateway.ApplyUpgradeAsync(diff.RoleId, diff.ToVersion));
        if (role is null)
        {
            return false;
        }

        Diff = null;
        Status = $"{role.Name} is at {role.PackVersion}.";
        await RefreshAsync();
        return true;
    }
}
