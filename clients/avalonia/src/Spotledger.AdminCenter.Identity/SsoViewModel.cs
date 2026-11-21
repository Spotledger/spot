using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// Single sign-on setup: OIDC and SAML provider forms (the client secret is write-only), a test
/// sign-in, and the tenant policy (local, optional, required). Making SSO required is refused while
/// fewer than two local break-glass administrators exist, so a broken provider cannot lock everyone out.
/// </summary>
public sealed class SsoViewModel : IdentityViewModel
{
    /// <summary>How many break-glass administrators must exist before SSO can be required.</summary>
    public const int MinimumBreakGlassAdministrators = 2;

    private IReadOnlyList<SsoProvider> _providers = [];
    private TenantAuthState _tenant = new(SsoPolicy.Local, 0);
    private SsoTestResult? _lastTest;

    /// <summary>Creates the screen.</summary>
    public SsoViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
    }

    /// <summary>The saved providers.</summary>
    public IReadOnlyList<SsoProvider> Providers
    {
        get => _providers;
        private set => SetField(ref _providers, value);
    }

    /// <summary>The tenant policy and break-glass count.</summary>
    public TenantAuthState Tenant
    {
        get => _tenant;
        private set
        {
            if (SetField(ref _tenant, value))
            {
                OnPropertyChanged(nameof(BreakGlassWarning));
            }
        }
    }

    /// <summary>The result of the last test sign-in.</summary>
    public SsoTestResult? LastTest
    {
        get => _lastTest;
        private set => SetField(ref _lastTest, value);
    }

    /// <summary>
    /// A warning for the screen while SSO is on but fewer than two break-glass administrators exist;
    /// null when all is well.
    /// </summary>
    public string? BreakGlassWarning =>
        Tenant.Policy != SsoPolicy.Local && Tenant.BreakGlassAdministrators < MinimumBreakGlassAdministrators
            ? $"Only {Tenant.BreakGlassAdministrators} break-glass administrator(s) exist. Create at least {MinimumBreakGlassAdministrators} local administrators before relying on single sign-on."
            : null;

    /// <summary>Loads providers and the tenant state.</summary>
    public async Task RefreshAsync()
    {
        var providers = await CallAsync(() => Gateway.ListSsoProvidersAsync());
        if (providers is not null)
        {
            Providers = providers;
        }

        var tenant = await CallAsync(() => Gateway.GetTenantAuthAsync());
        if (tenant is not null)
        {
            Tenant = tenant;
        }
    }

    /// <summary>Saves a provider form. The secret in the draft is sent once and the caller's copy is not kept here.</summary>
    public async Task<bool> SaveProviderAsync(SsoProviderDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        var problem = Validate(draft);
        if (problem is not null)
        {
            Error = problem;
            return false;
        }

        var saved = await CallAsync(() => Gateway.SaveSsoProviderAsync(draft));
        if (saved is null)
        {
            return false;
        }

        Status = $"Saved {saved.Name}. Test the sign-in before relying on it.";
        await RefreshAsync();
        return true;
    }

    private static string? Validate(SsoProviderDraft draft)
    {
        if (string.IsNullOrWhiteSpace(draft.Name))
        {
            return "Give the provider a name.";
        }

        if (!Uri.TryCreate(draft.Issuer, UriKind.Absolute, out var issuer) || issuer.Scheme != Uri.UriSchemeHttps)
        {
            return draft.Protocol == SsoProtocol.Oidc
                ? "The issuer must be an https address."
                : "The identity provider's entity id must be an https address.";
        }

        if (!string.IsNullOrWhiteSpace(draft.MetadataUrl)
            && (!Uri.TryCreate(draft.MetadataUrl, UriKind.Absolute, out var metadata) || metadata.Scheme != Uri.UriSchemeHttps))
        {
            return "The metadata address must be an https address.";
        }

        if (string.IsNullOrWhiteSpace(draft.ClientIdOrEntity))
        {
            return draft.Protocol == SsoProtocol.Oidc ? "Enter the client id." : "Enter the service provider entity id.";
        }

        if (draft.Protocol == SsoProtocol.Oidc && draft.Id is null && string.IsNullOrWhiteSpace(draft.ClientSecret))
        {
            return "Enter the client secret.";
        }

        return null;
    }

    /// <summary>Runs a test sign-in. A failed test is shown but is not an error of the screen.</summary>
    public async Task<bool> TestAsync(string providerId)
    {
        var result = await CallAsync(() => Gateway.TestSsoProviderAsync(providerId));
        LastTest = result;
        if (result is null)
        {
            return false;
        }

        Status = result.Succeeded ? "Test sign-in worked." : "Test sign-in failed.";
        return result.Succeeded;
    }

    /// <summary>Sets the tenant policy. Required needs two break-glass administrators and a successful provider.</summary>
    public async Task<bool> SetPolicyAsync(SsoPolicy policy)
    {
        if (policy == SsoPolicy.Required)
        {
            // Read the count fresh: it may have changed since the screen loaded.
            var current = await CallAsync(() => Gateway.GetTenantAuthAsync());
            if (current is null)
            {
                return false;
            }

            Tenant = current;
            if (current.BreakGlassAdministrators < MinimumBreakGlassAdministrators)
            {
                Error = $"Single sign-on cannot be required: only {current.BreakGlassAdministrators} break-glass administrator(s) exist, and at least {MinimumBreakGlassAdministrators} are needed.";
                return false;
            }

            if (Providers.Count == 0)
            {
                Error = "Add and test a provider before requiring single sign-on.";
                return false;
            }

            var confirmed = await Shell.ConfirmAsync(
                "Require single sign-on",
                "Everyone except break-glass administrators must sign in through the provider.",
                "Require");
            if (!confirmed)
            {
                return false;
            }
        }

        var state = await CallAsync(() => Gateway.SetSsoPolicyAsync(policy));
        if (state is null)
        {
            return false;
        }

        Tenant = state;
        Status = $"Sign-in policy is now {state.Policy}.";
        return true;
    }
}
