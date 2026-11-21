using Spotledger.Client.Api;

namespace Spotledger.Client.Shell;

/// <summary>
/// The stage machine the Desktop app binds to: server picker → login → shell, and back
/// on logout or a session-expired 401. Owns the three view-models and the one
/// <see cref="ApiConnection"/>; views are selected by <see cref="CurrentStage"/>.
/// WP-UI-02.T3: the picker's ready check also negotiates the server version; "Next"
/// attempts a start-up resume from the OS secret store before showing the login (a valid
/// remembered token lands directly in the shell, a stale one prefills the form), and a
/// mid-session 401 shows the re-sign-in modal over the shell — cancelling it returns the
/// user to the sign-in step with the open tabs preserved read-only.
/// </summary>
public sealed class AppViewModel : ObservableObject
{
    private AppStage _currentStage = AppStage.ServerPicker;
    private readonly ApiConnection _connection;
    private readonly SynchronizationContext? _uiContext = SynchronizationContext.Current;

    public AppViewModel(ApiConnection connection, RecentServersStore recentServers)
    {
        _connection = connection ?? throw new ArgumentNullException(nameof(connection));
        ArgumentNullException.ThrowIfNull(recentServers);
        ServerPicker = new ServerPickerViewModel(_connection, recentServers);
        Login = new LoginViewModel(_connection);
        Shell = new ShellViewModel(_connection);

        ServerPicker.ReadyToLogin += (_, _) => _ = OnReadyToLoginAsync();
        Login.BackRequested += (_, _) => CurrentStage = AppStage.ServerPicker;
        Login.SignedIn += (_, _) =>
        {
            CurrentStage = AppStage.Shell;
            SessionInitialization = InitializeShellAsync();
        };
        // A sign-in from anywhere (login step or later re-sign-in) ends any read-only spell.
        Login.SignedIn += (_, _) => Shell.MarkSignedInAgain();
        foreach (var command in new[] { ServerPicker.CheckCommand, Login.SignInCommand,
                     Shell.LogoutCommand, Shell.OpenWorkspaceCommand })
        {
            command.ExecutionFailed += _ => Shell.ReportSessionError("The request could not be completed. Try again.");
        }
        Shell.SignedOut += (_, _) => CurrentStage = AppStage.Login;
        Shell.SignInAgainRequested += (_, _) => CurrentStage = AppStage.Login;

        // Any authorized call answering 401 with a session-breaking code lands back on login.
        _connection.SessionExpired += (_, _) =>
        {
            // The mid-session prompt owns its own UX; the hard paths (session load right
            // after sign-in, direct helper calls) clear the shell and go to login.
            if (_connection.IsReSignInActive)
            {
                return;
            }

            // The API library raises this event on its network continuation. Return to
            // the context on which the app was created before touching bound state.
            if (_uiContext is not null && SynchronizationContext.Current != _uiContext)
            {
                _uiContext.Post(_ => Shell.HandleSessionExpired(), null);
            }
            else
            {
                Shell.HandleSessionExpired();
            }
        };

        // The aware-call wrapper asks for the modal sign-in over the current screen.
        _connection.ReSignInProvider = ShowReSignInAsync;
    }

    /// <summary>The mid-session modal: over the shell's current screen, tabs untouched. True = signed in again.</summary>
    private async Task<bool> ShowReSignInAsync()
    {
        return await Shell.OpenReSignInAsync().ConfigureAwait(true);
    }

    /// <summary>The picker accepted a ready server: try the start-up resume, then land on
    /// login (or straight into the shell). The fire-and-forget task is observed so no
    /// failure is lost; the stage change is the user-visible proof it ran.</summary>
    private async Task OnReadyToLoginAsync()
    {
        try
        {
            await RunResumeAndOpenLoginAsync().ConfigureAwait(true);
        }
        catch (Exception exception) when (exception is OperationCanceledException or ObjectDisposedException)
        {
            // Shutdown during the resume check: the stage stays on the picker.
        }
        catch (Exception)
        {
            CurrentStage = AppStage.Login;
        }
    }

    /// <summary>The current post-login load, including error handling; callers can await shutdown safely.</summary>
    public Task SessionInitialization { get; private set; } = Task.CompletedTask;

    /// <summary>The user a rejected resume prefilled (the app surfaces it to the host title bar).</summary>
    public string? ResumedRejectedUser => Login.PrefilledUser;

    private async Task InitializeShellAsync()
    {
        try
        {
            await Shell.InitializeAsync();
        }
        catch (Exception exception) when (exception is OperationCanceledException or ObjectDisposedException)
        {
            // Closing the connection during shutdown is an expected end to this load.
        }
        catch (Exception)
        {
            // Event-driven initialization must never leave an unobserved failure.
            Shell.ReportSessionError("The session could not be loaded. Try signing in again.");
        }
    }

    /// <summary>
    /// Runs from the picker's "Next": a start-up resume against a server the picker just
    /// proved ready. A resumed session goes straight to the shell; a rejected one shows
    /// the sign-in step with the server and user pre-filled; nothing remembered shows the
    /// plain sign-in step; an unreachable server keeps the resume pending and the user on
    /// the picker's error text.
    /// </summary>
    public async Task RunResumeAndOpenLoginAsync()
    {
        var result = await _connection.ResumeSessionAsync().ConfigureAwait(true);
        switch (result.Kind)
        {
            case SessionResumeKind.Resumed:
                CurrentStage = AppStage.Shell;
                Shell.ApplyResumedSession(result.User!, result.Session!);
                SessionInitialization = InitializeShellAsync();
                break;
            case SessionResumeKind.Rejected:
                Login.PrefillFromResume(ServerPicker.ServerUrl, result.User!);
                CurrentStage = AppStage.Login;
                break;
            case SessionResumeKind.Failed:
                Shell.ReportSessionError(result.Detail ?? "The session could not be checked.");
                CurrentStage = AppStage.Login;
                break;
            default:
                Login.ServerDisplay = ServerPicker.ServerUrl;
                CurrentStage = AppStage.Login;
                break;
        }
    }

    public AppStage CurrentStage
    {
        get => _currentStage;
        private set
        {
            if (SetField(ref _currentStage, value))
            {
                OnPropertyChanged(nameof(ShowServerPicker));
                OnPropertyChanged(nameof(ShowLogin));
                OnPropertyChanged(nameof(ShowShell));
            }
        }
    }

    public bool ShowServerPicker => CurrentStage == AppStage.ServerPicker;

    public bool ShowLogin => CurrentStage == AppStage.Login;

    public bool ShowShell => CurrentStage == AppStage.Shell;

    public ServerPickerViewModel ServerPicker { get; }

    public LoginViewModel Login { get; }

    public ShellViewModel Shell { get; }
}