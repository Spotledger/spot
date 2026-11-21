using System.Windows.Input;
using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// Users and their life cycle: list, search and filter, invite (single-use link), service users,
/// enable and disable with the leaver-actions preview, recovery actions behind a step-up and
/// anonymise behind a typed confirmation. The server's messages (the last administrator cannot be
/// disabled, say) are shown as they come.
/// </summary>
public sealed class UsersViewModel : IdentityViewModel
{
    private IReadOnlyList<UserSummary> _users = [];
    private UserSummary? _selected;
    private LeaverPreview? _leaverPreview;
    private string? _text;
    private UserState? _state;
    private UserKind? _kind;

    /// <summary>Creates the screen.</summary>
    public UsersViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
        RefreshCommand = new AsyncRelayCommand(_ => RefreshAsync());
    }

    /// <summary>Reloads the list.</summary>
    public ICommand RefreshCommand { get; }

    /// <summary>The users matching the filter.</summary>
    public IReadOnlyList<UserSummary> Users
    {
        get => _users;
        private set => SetField(ref _users, value);
    }

    /// <summary>The selected user.</summary>
    public UserSummary? Selected
    {
        get => _selected;
        set
        {
            if (SetField(ref _selected, value))
            {
                // A preview belongs to the user it was made for.
                LeaverPreview = null;
            }
        }
    }

    /// <summary>The filter text (name or email).</summary>
    public string? Text
    {
        get => _text;
        set => SetField(ref _text, value);
    }

    /// <summary>The state filter.</summary>
    public UserState? StateFilter
    {
        get => _state;
        set => SetField(ref _state, value);
    }

    /// <summary>The kind filter.</summary>
    public UserKind? KindFilter
    {
        get => _kind;
        set => SetField(ref _kind, value);
    }

    /// <summary>What disabling the selected user would take with it; set by <see cref="PreviewDisableAsync"/>.</summary>
    public LeaverPreview? LeaverPreview
    {
        get => _leaverPreview;
        private set => SetField(ref _leaverPreview, value);
    }

    /// <summary>Loads the list with the current filter.</summary>
    public async Task RefreshAsync()
    {
        var users = await CallAsync(() => Gateway.ListUsersAsync(new UserFilter(Text, StateFilter, KindFilter)));
        if (users is not null)
        {
            Users = users;
            if (Selected is { } selected)
            {
                Selected = users.FirstOrDefault(u => u.Id == selected.Id);
            }
        }
    }

    /// <summary>Invites a person. The single-use link is shown once (see <see cref="IdentityViewModel.RevealedSecret"/>).</summary>
    public async Task<bool> InviteAsync(InviteRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (string.IsNullOrWhiteSpace(request.Email) || !request.Email.Contains('@', StringComparison.Ordinal))
        {
            Error = "Enter the person's email address.";
            return false;
        }

        if (string.IsNullOrWhiteSpace(request.Name))
        {
            Error = "Enter the person's name.";
            return false;
        }

        var link = await CallAsync(() => Gateway.InviteUserAsync(request));
        if (link is null)
        {
            return false;
        }

        Reveal(link);
        Status = $"Invitation created for {request.Email}. The link works once.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Creates a service user owned by a person. Its first API key is shown once.</summary>
    public async Task<bool> CreateServiceUserAsync(string name, string ownerUserId)
    {
        if (string.IsNullOrWhiteSpace(name) || string.IsNullOrWhiteSpace(ownerUserId))
        {
            Error = "A service user needs a name and an owner.";
            return false;
        }

        var key = await CallAsync(() => Gateway.CreateServiceUserAsync(name, ownerUserId));
        if (key is null)
        {
            return false;
        }

        Reveal(key);
        Status = $"Service user {name} created.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Loads the leaver-actions preview for a user. Nothing is changed.</summary>
    public async Task<bool> PreviewDisableAsync(string userId)
    {
        var preview = await CallAsync(() => Gateway.PreviewLeaverAsync(userId));
        LeaverPreview = preview;
        return preview is not null;
    }

    /// <summary>
    /// Disables a user after the preview was loaded and the person confirmed it. Disabling without a
    /// preview is refused here; the server's last-administrator refusal is shown as it is.
    /// </summary>
    public async Task<bool> DisableAsync(string userId)
    {
        if (LeaverPreview is not { } preview || preview.UserId != userId)
        {
            Error = "Review what disabling this user will do first.";
            return false;
        }

        var confirmed = await Shell.ConfirmAsync(
            "Disable user",
            $"{preview.Sessions} sessions end, {preview.ApiKeys} API keys are revoked, {preview.Invitations} invitations are withdrawn and {preview.Tasks.Count} tasks are reassigned.",
            "Disable");
        if (!confirmed)
        {
            return false;
        }

        var done = await TryAsync(() => Gateway.DisableUserAsync(userId, preview.Tasks));
        if (!done)
        {
            return false;
        }

        LeaverPreview = null;
        Status = "User disabled.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Enables a disabled user.</summary>
    public async Task<bool> EnableAsync(string userId)
    {
        if (!await Shell.ConfirmAsync("Enable user", "This person will be able to sign in again.", "Enable"))
        {
            return false;
        }

        var done = await TryAsync(() => Gateway.EnableUserAsync(userId));
        if (!done)
        {
            return false;
        }

        Status = "User enabled.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Runs a recovery action; the person is asked to step up when the server wants it.</summary>
    public async Task<bool> RecoverAsync(string userId, RecoveryKind kind)
    {
        var what = kind switch
        {
            RecoveryKind.ResetPassword => "Reset this user's password?",
            RecoveryKind.ResetSecondFactor => "Clear this user's second factor?",
            RecoveryKind.EndSessions => "End all of this user's sessions?",
            _ => "Revoke all of this user's API keys?",
        };
        if (!await Shell.ConfirmAsync("Recover account", what, "Continue"))
        {
            return false;
        }

        var ok = await WithStepUpAsync(
            "Confirm it is you before recovering this account.",
            proof => Gateway.RecoverUserAsync(userId, kind, proof));
        if (ok)
        {
            Status = kind switch
            {
                RecoveryKind.ResetPassword => "Password reset sent.",
                RecoveryKind.ResetSecondFactor => "Second factor cleared.",
                RecoveryKind.EndSessions => "Sessions ended.",
                _ => "API keys revoked.",
            };
        }

        return ok;
    }

    /// <summary>
    /// Anonymises a user. The person must type the user's email (or id for a service user) exactly;
    /// a different text sends nothing.
    /// </summary>
    public async Task<bool> AnonymiseAsync(UserSummary user)
    {
        ArgumentNullException.ThrowIfNull(user);
        var expected = user.Kind == UserKind.Person ? user.Email : user.Id;
        var typed = await Shell.AskTextAsync(
            "Anonymise user",
            $"This erases {user.Name}'s personal data for good. Type {expected} to confirm.");
        if (typed is null)
        {
            return false;
        }

        if (!string.Equals(typed.Trim(), expected, StringComparison.Ordinal))
        {
            Error = "What you typed does not match. Nothing was changed.";
            return false;
        }

        var ok = await WithStepUpAsync(
            "Confirm it is you before anonymising a user.",
            proof => Gateway.AnonymiseUserAsync(user.Id, proof));
        if (ok)
        {
            Status = "User anonymised.";
            await RefreshAsync();
        }

        return ok;
    }
}
