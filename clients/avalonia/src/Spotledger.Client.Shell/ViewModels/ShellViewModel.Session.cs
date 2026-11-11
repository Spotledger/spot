namespace Spotledger.Client.Shell;

using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Models;

/// <summary>
/// The session-grade half of <see cref="ShellViewModel"/> (WP-UI-02.T3): the mid-session
/// re-sign-in modal over the current screen, the read-only state of a cancelled prompt
/// (tabs preserved in memory until the user signs back in or closes them), the change
/// password and My sessions dialogs behind the server's capabilities, and the API version
/// notice in About.
/// </summary>
public sealed partial class ShellViewModel
{
    public const string VersionNotice = "Version notice";

    /// <summary>Commands that keep working after a cancelled re-sign-in (tabs preserved
    /// read-only): closing, pinning and the menu navigation helpers. Registered here
    /// rather than in the command catalog so this task stays inside its owned paths.</summary>
    internal static readonly string[] ReadOnlySurvivingCommands =
    [
        "tab.close", "tab.others", "tab.all", "tab.pin", "tab.next", "tab.previous",
        "tab.focus", "view.sidebar", "shell.launcher", "help.about", "window.menu",
        "shell.signinAgain",
    ];

    /// <summary>
    /// Adds the task's session commands to the registry (Tools menu) from the view-model
    /// side, because `Commands/` is outside this task's owned paths. Mnemonics avoid the
    /// Tools level's existing K/D/R; the registry's own collision check enforces it.
    /// </summary>
    private void RegisterSessionCommands()
    {
        Registry.Add(new CommandDefinition("shell.changePassword", "Change password…", "Set a new password for this user",
            "Open", "Tools", 'P', null) { SearchTerms = ["passwd", "credentials"] });
        Registry.Add(new CommandDefinition("shell.mySessions", "My sessions…", "Review and end this user's open sessions",
            "Open", "Tools", 'S', null) { SearchTerms = ["devices", "sign out everywhere"] });
        Registry.Add(new CommandDefinition("shell.signinAgain", "Sign in again…", "Return to the sign-in step",
            "Open", "Tools", 'G', null));
    }

    private bool _isReSignInOpen;
    private bool _isReadOnly;
    private ChangePasswordViewModel? _changePassword;
    private MySessionsViewModel? _mySessions;
    private bool _isChangePasswordOpen;
    private bool _isMySessionsOpen;
    private TaskCompletionSource<bool>? _reSignInPrompt;

    /// <summary>True while the modal re-sign-in shows over the shell's current screen.</summary>
    public bool IsReSignInOpen
    {
        get => _isReSignInOpen;
        private set
        {
            if (SetField(ref _isReSignInOpen, value))
            {
                OnPropertyChanged(nameof(ShowReSignIn));
            }
        }
    }

    public bool ShowReSignIn => IsReSignInOpen;

    /// <summary>The user field of the modal (prefilled with the session user).</summary>
    public string ReSignInUser { get; set; } = string.Empty;

    /// <summary>The modal's password field; dropped when the modal closes, whatever the outcome.</summary>
    public string ReSignInPassword { get; set; } = string.Empty;

    /// <summary>The modal's error text (the contract's auth.* message).</summary>
    public string? ReSignInMessage { get; private set; }

    public bool HasReSignInMessage => !string.IsNullOrEmpty(ReSignInMessage);

    /// <summary>True while the modal's sign-in runs.</summary>
    public bool IsReSignInBusy { get; private set; }

    /// <summary>
    /// Read-only state after a cancelled re-sign-in: the tabs stay open in memory but the
    /// workspaces refuse to be driven until the user signs back in (or closes the tabs).
    /// </summary>
    public bool IsReadOnly
    {
        get => _isReadOnly;
        private set
        {
            if (SetField(ref _isReadOnly, value))
            {
                RaiseCommandsStateChanged();
            }
        }
    }

    /// <summary>The change-password dialog, null while closed.</summary>
    public ChangePasswordViewModel? ChangePassword
    {
        get => _changePassword;
        private set
        {
            if (SetField(ref _changePassword, value))
            {
                OnPropertyChanged(nameof(HasChangePassword));
            }
        }
    }

    public bool HasChangePassword => ChangePassword is not null;

    /// <summary>The My sessions dialog, null while closed.</summary>
    public MySessionsViewModel? MySessions
    {
        get => _mySessions;
        private set
        {
            if (SetField(ref _mySessions, value))
            {
                OnPropertyChanged(nameof(HasMySessions));
            }
        }
    }

    public bool HasMySessions => MySessions is not null;

    /// <summary>The shell-owned version notice (set from the picker's negotiation result).</summary>
    public string? ApiVersionNotice { get; private set; }

    /// <summary>Shown on the modal and read by the view: signed-in users see their name; busy state.</summary>
    public bool ChangePasswordVisible
    {
        get => _isChangePasswordOpen;
        private set => SetField(ref _isChangePasswordOpen, value);
    }

    /// <summary>True while the My sessions dialog shows (the view hosts it as an overlay).</summary>
    public bool MySessionsVisible
    {
        get => _isMySessionsOpen;
        private set => SetField(ref _isMySessionsOpen, value);
    }

    /// <summary>The modal's sign-in button, run by the view.</summary>
    public AsyncRelayCommand ReSignInCommand { get; private set; } = null!;

    /// <summary>The modal's cancel button: keep the shell read-only and return to the sign-in step.</summary>
    public RelayCommand CancelReSignInCommand { get; private set; } = null!;

    /// <summary>Raised when the modal opens or closes (the view shows/hides the overlay).</summary>
    public event EventHandler? ReSignInOpenChanged;

    /// <summary>Raised when the user chose to sign back in from the read-only state (the app switches stage).</summary>
    public event EventHandler? SignInAgainRequested;

    /// <summary>The re-sign-in entry the ApiConnection's aware wrapper awaits. Idempotent while open.</summary>
    public Task<bool> OpenReSignInAsync()
    {
        if (_reSignInPrompt is { } pending && !pending.Task.IsCompleted)
        {
            return pending.Task;
        }

        IsReSignInOpen = true;
        ReSignInCommand.RaiseCanExecuteChanged();
        ReSignInOpenChanged?.Invoke(this, EventArgs.Empty);
        _reSignInPrompt = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        return _reSignInPrompt.Task;
    }

    /// <summary>The modal's sign-in: logs in again over the same connection (the token swaps in place).</summary>
    private async Task ReSignInAsync(object? parameter)
    {
        if (IsReSignInBusy)
        {
            return;
        }

        IsReSignInBusy = true;
        ReSignInMessage = null;
        var user = ReSignInUser;
        var password = ReSignInPassword;
        try
        {
            await _connection.LoginAsync(user, password).ConfigureAwait(true);
            ReSignInPassword = string.Empty;
            FinishReSignIn(signedIn: true);
        }
        catch (ApiRequestException failure)
        {
            ReSignInPassword = string.Empty;
            ReSignInMessage = failure.Error.Message;
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            ReSignInPassword = string.Empty;
            ReSignInMessage = "The sign-in request could not be completed. Try again.";
        }
        finally
        {
            IsReSignInBusy = false;
        }
    }

    /// <summary>The modal's cancel: back to the sign-in step, tabs preserved read-only.</summary>
    private void CancelReSignIn()
    {
        ReSignInPassword = string.Empty;
        FinishReSignIn(signedIn: false);
    }

    private void FinishReSignIn(bool signedIn)
    {
        IsReSignInOpen = false;
        IsReSignInBusy = false;
        if (signedIn)
        {
            IsReadOnly = false;
            StatusMessage = "Signed in again.";
        }
        else
        {
            IsReadOnly = true;
            StatusMessage = "Sign in again to keep working.";
        }

        ReSignInOpenChanged?.Invoke(this, EventArgs.Empty);
        _reSignInPrompt?.TrySetResult(signedIn);
        _reSignInPrompt = null;
        if (!signedIn)
        {
            SignInAgainRequested?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>A resumed session (the start-up resume): fills the header without reloading the form.</summary>
    public void ApplyResumedSession(string user, SessionInfo session)
    {
        UserName = session.User?.FullName ?? user;
        UserEmail = session.User?.Email ?? string.Empty;
        UserId = session.User?.Id ?? string.Empty;
        SiteName = session.Site ?? string.Empty;
        ExpiresAt = session.ExpiresAt ?? string.Empty;
        SessionError = null;
        RaiseCommandsStateChanged();
    }

    /// <summary>The user leaves the read-only state by asking for the sign-in step (the app switches stage).</summary>
    public void RequestSignInAgain()
    {
        SignInAgainRequested?.Invoke(this, EventArgs.Empty);
    }

    // ---- capability-gated dialogs ----

    /// <summary>A successful sign-in ends a cancelled prompt's read-only state (the shell drives again).</summary>
    public void MarkSignedInAgain() => IsReadOnly = false;

    /// <summary>Opens the change-password dialog when the server supports it; otherwise the command stays restricted (menu grey-out).</summary>
    public async Task OpenChangePasswordAsync()
    {
        if (!_connection.Capabilities.PasswordChange)
        {
            return;
        }

        ChangePassword = new ChangePasswordViewModel(_connection);
        ChangePassword.CloseRequested += (_, _) => CloseChangePassword();
        ChangePasswordVisible = true;
        await Task.CompletedTask.ConfigureAwait(true);
    }

    public void CloseChangePassword()
    {
        ChangePasswordVisible = false;
        ChangePassword = null;
    }

    /// <summary>Opens the My sessions dialog when the server supports it and loads the list once (no polling).</summary>
    public async Task OpenMySessionsAsync()
    {
        if (!_connection.Capabilities.SessionManagement)
        {
            return;
        }

        MySessions = new MySessionsViewModel(_connection);
        MySessions.CloseRequested += (_, _) => CloseMySessions();
        MySessionsVisible = true;
        await MySessions.LoadAsync().ConfigureAwait(true);
    }

    public void CloseMySessions()
    {
        MySessionsVisible = false;
        MySessions = null;
    }

    /// <summary>The version notice for About (the picker's non-blocking newer-minor notice).</summary>
    public void PublishVersionNotice(string? notice)
    {
        ApiVersionNotice = notice;
    }
}