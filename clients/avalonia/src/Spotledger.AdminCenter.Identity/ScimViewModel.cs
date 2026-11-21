using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// SCIM provisioning: the endpoint, token management (a new token is shown once and never again)
/// and the group-mapping table with the list of groups the identity provider sent that nothing maps.
/// </summary>
public sealed class ScimViewModel : IdentityViewModel
{
    private ScimEndpoint? _endpoint;
    private IReadOnlyList<ScimTokenInfo> _tokens = [];
    private List<GroupMapping> _mappings = [];
    private IReadOnlyList<string> _unmapped = [];

    /// <summary>Creates the screen.</summary>
    public ScimViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
    }

    /// <summary>The endpoint the identity provider calls.</summary>
    public ScimEndpoint? Endpoint
    {
        get => _endpoint;
        private set => SetField(ref _endpoint, value);
    }

    /// <summary>Token records (never token values).</summary>
    public IReadOnlyList<ScimTokenInfo> Tokens
    {
        get => _tokens;
        private set => SetField(ref _tokens, value);
    }

    /// <summary>The mapping table as edited. Saved with <see cref="SaveMappingsAsync"/>.</summary>
    public IReadOnlyList<GroupMapping> Mappings => _mappings;

    /// <summary>Groups the identity provider sent that no mapping covers.</summary>
    public IReadOnlyList<string> UnmappedGroups
    {
        get => _unmapped;
        private set
        {
            if (SetField(ref _unmapped, value))
            {
                OnPropertyChanged(nameof(UnmappedWarning));
            }
        }
    }

    /// <summary>The warning shown above the table while groups are unmapped; null when none.</summary>
    public string? UnmappedWarning => UnmappedGroups.Count == 0
        ? null
        : $"{UnmappedGroups.Count} group(s) from your identity provider have no mapping, so their members get no access: {string.Join(", ", UnmappedGroups)}.";

    /// <summary>True when the table has edits that are not saved.</summary>
    public bool HasUnsavedMappings { get; private set; }

    /// <summary>Loads the endpoint, tokens and mappings.</summary>
    public async Task RefreshAsync()
    {
        var scim = await CallAsync(() => Gateway.GetScimAsync());
        if (scim is { } value)
        {
            Endpoint = value.Endpoint;
            Tokens = value.Tokens;
        }

        var maps = await CallAsync(() => Gateway.GetGroupMappingsAsync());
        if (maps is { } loaded)
        {
            _mappings = [.. loaded.Mappings];
            HasUnsavedMappings = false;
            OnPropertyChanged(nameof(Mappings));
            UnmappedGroups = loaded.Unmapped;
        }
    }

    /// <summary>Creates a token behind a step-up. The value is shown once through <see cref="IdentityViewModel.RevealedSecret"/>.</summary>
    public async Task<bool> CreateTokenAsync()
    {
        var secret = await WithStepUpAsync(
            "Confirm it is you before creating a provisioning token.",
            proof => Gateway.CreateScimTokenAsync(proof));
        if (secret is null)
        {
            return false;
        }

        Reveal(secret);
        Status = "Token created. Copy it now: it will not be shown again.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Revokes a token behind a step-up and a confirmation.</summary>
    public async Task<bool> RevokeTokenAsync(string tokenId)
    {
        var confirmed = await Shell.ConfirmAsync(
            "Revoke token",
            "Provisioning with this token stops at once.",
            "Revoke");
        if (!confirmed)
        {
            return false;
        }

        var ok = await WithStepUpAsync(
            "Confirm it is you before revoking a provisioning token.",
            proof => Gateway.RevokeScimTokenAsync(tokenId, proof));
        if (ok)
        {
            Status = "Token revoked.";
            await RefreshAsync();
        }

        return ok;
    }

    /// <summary>Adds a row to the mapping table. The group must be new and the role set.</summary>
    public bool AddMapping(GroupMapping mapping)
    {
        ArgumentNullException.ThrowIfNull(mapping);
        if (string.IsNullOrWhiteSpace(mapping.Group) || string.IsNullOrWhiteSpace(mapping.RoleId))
        {
            Error = "A mapping needs a group and a role.";
            return false;
        }

        if (_mappings.Exists(m => string.Equals(m.Group, mapping.Group, StringComparison.OrdinalIgnoreCase)
                                  && m.Scope == mapping.Scope && m.RoleId == mapping.RoleId))
        {
            Error = $"{mapping.Group} is already mapped to that role and scope.";
            return false;
        }

        Error = null;
        _mappings.Add(mapping);
        // A group that is now mapped leaves the warning list.
        UnmappedGroups = [.. UnmappedGroups.Where(g => !string.Equals(g, mapping.Group, StringComparison.OrdinalIgnoreCase))];
        HasUnsavedMappings = true;
        OnPropertyChanged(nameof(Mappings));
        return true;
    }

    /// <summary>Removes a row from the table.</summary>
    public bool RemoveMapping(GroupMapping mapping)
    {
        var removed = _mappings.Remove(mapping);
        if (removed)
        {
            HasUnsavedMappings = true;
            OnPropertyChanged(nameof(Mappings));
        }

        return removed;
    }

    /// <summary>Saves the table, reloads it, and so refreshes the unmapped-groups warning from the server.</summary>
    public async Task<bool> SaveMappingsAsync()
    {
        var ok = await TryAsync(() => Gateway.SaveGroupMappingsAsync(_mappings));
        if (!ok)
        {
            return false;
        }

        Status = "Group mappings saved.";
        await RefreshAsync();
        return true;
    }
}
