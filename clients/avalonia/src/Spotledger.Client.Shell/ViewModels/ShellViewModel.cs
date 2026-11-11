using System.Collections.ObjectModel;
using System.Collections.Specialized;
using Spotledger.Client.Api;

namespace Spotledger.Client.Shell;

/// <summary>The sidebar states cycled by Ctrl+B and the toggle button.</summary>
public enum SidebarMode
{
    Expanded,
    IconsOnly,
    Hidden,
}

/// <summary>
/// Step three: the real application chrome after login. Loads <c>GET /api/v1/auth/session</c>
/// for the user chip, and drives the menu bar, tabs, sidebar, Ctrl+K launcher, New button,
/// notification bell and status bar through the one command model
/// (<see cref="CommandRegistry"/> + <see cref="CommandDispatcher"/>). Logout stays
/// <c>POST /api/v1/auth/logout</c> — locally always, even when the server call fails.
/// </summary>
public sealed partial class ShellViewModel : ObservableObject
{
    private string _userName = string.Empty;
    private string _userEmail = string.Empty;
    private string _userId = string.Empty;
    private string _siteName = string.Empty;
    private string _expiresAt = string.Empty;
    private string? _sessionError;
    private WorkspaceTabViewModel? _selectedTab;
    private string _selectedWorkspace = "Home";
    private bool _rightToLeft;
    private bool _compactDensity;
    private SidebarMode _sidebarMode = SidebarMode.Expanded;
    private bool _isLauncherOpen;
    private bool _isMenuOpen;
    private int _notificationCount;
    private string _statusMessage = "Ready";
    private readonly ApiConnection _connection;

    /// <summary>The workspace labels of the rail (kept from T1 so its tests stay green).</summary>
    public static readonly string[] Workspaces = ShellCommands.Workspaces.Select(w => w.Label).ToArray();

    public ShellViewModel(ApiConnection connection)
    {
        _connection = connection ?? throw new ArgumentNullException(nameof(connection));
        Registry = ShellCommands.BuildRegistry();
        RegisterSessionCommands();
        Dispatcher = new CommandDispatcher(Registry);
        Dispatcher.Availability = ComputeAvailability;
        Launcher = new LauncherViewModel(Registry, Dispatcher)
        {
            ContextProvider = CurrentContext,
            Recents = Tabs,
        };
        Tabs.CollectionChanged += OnTabsChanged;
        Keymap = KeymapProfile.Load("desktop");
        DesktopKeymap = Keymap;
        LogoutCommand = new AsyncRelayCommand(LogoutAsync);
        OpenWorkspaceCommand = new AsyncRelayCommand(async parameter =>
        {
            if (parameter is string workspace)
            {
                await OpenTabAsync(workspace);
            }
        });
        CloseTabCommand = new RelayCommand(parameter =>
        {
            if (parameter is WorkspaceTabViewModel tab)
            {
                CloseTab(tab);
            }
        });
        SelectNextTabCommand = new RelayCommand(_ => CycleTab(forward: true));
        CloseActiveTabCommand = new RelayCommand(_ =>
        {
            if (SelectedTab is { } active)
            {
                CloseTab(active);
            }
        });
        RegisterChromeHandlers();
        BuildShellCommands();
        ReSignInCommand = new AsyncRelayCommand(ReSignInAsync, _ => !IsReSignInBusy);
        CancelReSignInCommand = new RelayCommand(_ => CancelReSignIn());
    }

    /// <summary>Raised when logout completes (even if the server call failed): return to login.</summary>
    public event EventHandler? SignedOut;

    /// <summary>Raised when the chrome state a menu grey-out depends on changed.</summary>
    public event EventHandler? CommandsStateChanged;

    /// <summary>Set by the view: asked when a closing tab is dirty and the decision is needed.</summary>
    public Func<WorkspaceTabViewModel, Task<TabCloseChoice>>? ClosePromptRequested { get; set; }

    /// <summary>Raised when the launcher opens or closes; the view moves focus accordingly.</summary>
    public event EventHandler? LauncherOpenChanged;

    /// <summary>Raised when the view must run a clipboard action on the focused text field.</summary>
    public event Action<string>? ClipboardActionRequested;

    /// <summary>Raised when Ctrl+B or the toggle changes the sidebar mode (view animates widths).</summary>
    public event EventHandler? SidebarModeChanged;

    public ObservableCollection<WorkspaceTabViewModel> Tabs { get; } = new();

    public CommandRegistry Registry { get; }

    public CommandDispatcher Dispatcher { get; }

    public LauncherViewModel Launcher { get; }

    /// <summary>Menu, launcher and status bar share these command objects.</summary>
    public IReadOnlyDictionary<string, ShellCommand> Commands { get; private set; } =
        new Dictionary<string, ShellCommand>();

    /// <summary>The active keyboard profile. The desktop host keeps <c>desktop</c>.</summary>
    public KeymapProfile Keymap { get; private set; }

    /// <summary>The desktop profile, shown to the user when a browser profile is active.</summary>
    public KeymapProfile DesktopKeymap { get; private set; }

    /// <summary>The session user's full name.</summary>
    public string UserName
    {
        get => _userName;
        private set => SetField(ref _userName, value);
    }

    /// <summary>The session user's id (the server's record id); empty before the session loads and after sign-out.
    /// Draft storage is scoped by it, so a different user never sees these drafts.</summary>
    public string UserId
    {
        get => _userId;
        private set => SetField(ref _userId, value);
    }

    /// <summary>The session user's e-mail.</summary>
    public string UserEmail
    {
        get => _userEmail;
        private set => SetField(ref _userEmail, value);
    }

    /// <summary>The site name from the session response, empty when the server omits it.</summary>
    public string SiteName
    {
        get => _siteName;
        private set => SetField(ref _siteName, value);
    }

    /// <summary>When the session expires (the contract's UTC stamp, shown as-is).</summary>
    public string ExpiresAt
    {
        get => _expiresAt;
        private set => SetField(ref _expiresAt, value);
    }

    /// <summary>A load-session problem shown in the content area (a 401 lands the user back on login).</summary>
    public string? SessionError
    {
        get => _sessionError;
        private set
        {
            SetField(ref _sessionError, value);
            OnPropertyChanged(nameof(HasSessionError));
        }
    }

    public bool HasSessionError => !string.IsNullOrEmpty(SessionError);

    public WorkspaceTabViewModel? SelectedTab
    {
        get => _selectedTab;
        set
        {
            if (SetField(ref _selectedTab, value))
            {
                CloseActiveTabCommand.RaiseCanExecuteChanged();
                RaiseCommandsStateChanged();
            }
        }
    }

    /// <summary>The highlighted sidebar workspace; opening one reuses or creates its tab.</summary>
    public string SelectedWorkspace
    {
        get => _selectedWorkspace;
        private set => SetField(ref _selectedWorkspace, value);
    }

    /// <summary>Content direction switch shown in Settings (RTL support).</summary>
    public bool RightToLeft
    {
        get => _rightToLeft;
        set
        {
            if (SetField(ref _rightToLeft, value))
            {
                OnPropertyChanged(nameof(FlowDirection));
            }
        }
    }

    /// <summary>The Avalonia FlowDirection matching <see cref="RightToLeft"/>.</summary>
    public Avalonia.Media.FlowDirection FlowDirection => RightToLeft
        ? Avalonia.Media.FlowDirection.RightToLeft
        : Avalonia.Media.FlowDirection.LeftToRight;

    /// <summary>Compact density switch (Tools → Toggle density).</summary>
    public bool CompactDensity
    {
        get => _compactDensity;
        set
        {
            if (SetField(ref _compactDensity, value))
            {
                OnPropertyChanged(nameof(DensityLabel));
            }
        }
    }

    public string DensityLabel => CompactDensity ? "Compact" : "Comfortable";

    /// <summary>The sidebar state cycled by Ctrl+B.</summary>
    public SidebarMode Sidebar
    {
        get => _sidebarMode;
        set
        {
            if (SetField(ref _sidebarMode, value))
            {
                OnPropertyChanged(nameof(IsSidebarExpanded));
                OnPropertyChanged(nameof(IsSidebarIconsOnly));
                OnPropertyChanged(nameof(IsSidebarHidden));
                SidebarModeChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    public bool IsSidebarExpanded => Sidebar == SidebarMode.Expanded;

    public bool IsSidebarIconsOnly => Sidebar == SidebarMode.IconsOnly;

    public bool IsSidebarHidden => Sidebar == SidebarMode.Hidden;

    /// <summary>True while the Ctrl+K palette is open.</summary>
    public bool IsLauncherOpen
    {
        get => _isLauncherOpen;
        private set
        {
            if (SetField(ref _isLauncherOpen, value))
            {
                LauncherOpenChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    /// <summary>True while a menu bar dropdown is open (status bar hint row).</summary>
    public bool IsMenuOpen
    {
        get => _isMenuOpen;
        set => SetField(ref _isMenuOpen, value);
    }

    /// <summary>Unread notifications shown on the bell badge; 0 hides the badge.</summary>
    public int NotificationCount
    {
        get => _notificationCount;
        set
        {
            if (SetField(ref _notificationCount, value))
            {
                OnPropertyChanged(nameof(HasNotifications));
            }
        }
    }

    public bool HasNotifications => NotificationCount > 0;

    /// <summary>Status bar text (connection, sync and demo state arrive with later tasks).</summary>
    public string StatusMessage
    {
        get => _statusMessage;
        private set => SetField(ref _statusMessage, value);
    }

    public AsyncRelayCommand LogoutCommand { get; }

    public AsyncRelayCommand OpenWorkspaceCommand { get; }

    public RelayCommand CloseTabCommand { get; }

    public RelayCommand SelectNextTabCommand { get; }

    public RelayCommand CloseActiveTabCommand { get; }

    /// <summary>Loads the session and opens the first workspace. Call once after login.</summary>
    public async Task InitializeAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            var session = await _connection.LoadSessionAsync(cancellationToken);
            UserName = session.User?.FullName ?? string.Empty;
            UserEmail = session.User?.Email ?? string.Empty;
            UserId = session.User?.Id ?? string.Empty;
            SiteName = session.Site ?? string.Empty;
            ExpiresAt = session.ExpiresAt ?? string.Empty;
            SessionError = null;
        }
        catch (ApiRequestException failure)
        {
            // A 401 here already cleared the token and raised SessionExpired; the app is on
            // its way back to login. Show the contract text meanwhile.
            SessionError = failure.Error.Message;
        }

        catch (Exception exception) when (exception is OperationCanceledException or ObjectDisposedException)
        {
            // The connection may be closed while the request is in flight during shutdown.
            return;
        }
        catch (HttpRequestException)
        {
            SessionError = "The session could not be loaded. Check the connection and try signing in again.";
            return;
        }

        // Do not seed the first tab once the session is gone: HandleSessionExpired has
        // already cleared the tabs and the stage is back on login.
        if (Tabs.Count == 0 && _connection.Tokens.HasToken)
        {
            await OpenTabAsync("Home", cancellationToken);
        }

        RaiseCommandsStateChanged();
    }

    /// <summary>Surfaces unexpected failures from the app's asynchronous entry points.</summary>
    internal void ReportSessionError(string message) => SessionError = message;

    /// <summary>The command context the availability checks read (shell state snapshot).</summary>
    public CommandContext CurrentContext() => new(
        ActiveTab: SelectedTab,
        HasActiveTab: SelectedTab is not null,
        ActiveTabDirty: SelectedTab?.IsDirty == true,
        HasFocusedText: IsFocusedText);

    /// <summary>Set by the view: true while a text input holds focus (Edit menu grey-out).</summary>
    public bool IsFocusedText
    {
        get => _isFocusedText;
        set
        {
            if (SetField(ref _isFocusedText, value))
            {
                RaiseCommandsStateChanged();
            }
        }
    }

    private bool _isFocusedText;

    /// <summary>The local availability rules (ADR-055 seam: the server answers these later).</summary>
    private CommandAvailability ComputeAvailability(CommandDefinition definition, CommandContext context)
    {
        if (IsReadOnly && !ReadOnlySurvivingCommands.Contains(definition.Id, StringComparer.Ordinal))
        {
            // After a cancelled re-sign-in the shell keeps the tabs but refuses to drive
            // them until the user signs back in; closing and pinning stay usable.
            return CommandAvailability.Restricted("Sign in again to keep working.");
        }
        if (ShellCommands.DocumentCreateCommands.Contains(definition.Id, StringComparer.Ordinal))
        {
            return CommandAvailability.Blocked("The form for this document arrives with a later task.");
        }

        if (string.Equals(definition.Id, "process.cases", StringComparison.Ordinal))
        {
            return CommandAvailability.Blocked("The process case runtime arrives with WP-BPM-08.");
        }

        if (string.Equals(definition.Id, "report.purchases", StringComparison.Ordinal))
        {
            return CommandAvailability.Blocked("The report runtime arrives with WP-UI-10.");
        }

        if (ShellCommands.TabScopedCommands.Contains(definition.Id, StringComparer.Ordinal)
            && !context.HasActiveTab)
        {
            return CommandAvailability.Blocked("No tab is open.");
        }

        if (ShellCommands.TextScopedCommands.Contains(definition.Id, StringComparer.Ordinal)
            && !context.HasFocusedText)
        {
            return CommandAvailability.NeedsContext("Focus a text field first.");
        }

        if (string.Equals(definition.Id, "tab.newWindow", StringComparison.Ordinal))
        {
            return CommandAvailability.Blocked("Multi-window support arrives with a later task.");
        }

        // The session dialogs are hidden against a server that lacks the contract-0.3.0
        // auth features (capability checks, not scattered version checks).
        if (string.Equals(definition.Id, "shell.changePassword", StringComparison.Ordinal))
        {
            return _connection.Capabilities.PasswordChange
                ? CommandAvailability.Available
                : CommandAvailability.Restricted("This server's API version has no password change.");
        }

        if (string.Equals(definition.Id, "shell.mySessions", StringComparison.Ordinal))
        {
            return _connection.Capabilities.SessionManagement
                ? CommandAvailability.Available
                : CommandAvailability.Restricted("This server's API version has no session management.");
        }

        return CommandAvailability.Available;
    }

    /// <summary>Opens (or focuses) the tab for a rail workspace.</summary>
    public Task OpenTabAsync(string workspace, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(workspace);
        var (commandId, _) = ShellCommands.Workspaces.FirstOrDefault(w =>
            string.Equals(w.Label, workspace, StringComparison.Ordinal));
        var description = WorkspaceDescriptions.For(workspace);
        var existing = Tabs.FirstOrDefault(tab => string.Equals(tab.Workspace, workspace, StringComparison.Ordinal));
        if (existing is not null)
        {
            SelectedTab = existing;
            SelectedWorkspace = workspace;
            return Task.CompletedTask;
        }

        var tab = new WorkspaceTabViewModel(workspace) { Description = description };
        Tabs.Add(tab);
        SelectedTab = tab;
        SelectedWorkspace = workspace;
        _ = commandId;
        return Task.CompletedTask;
    }

    /// <summary>Opens (or focuses) a document tab by record UUID (ADR-33).</summary>
    public WorkspaceTabViewModel OpenDocumentTab(
        string workspace, Guid documentId, string title, string? documentNumber = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(workspace);
        var existing = Tabs.FirstOrDefault(tab => tab.DocumentId == documentId);
        if (existing is not null)
        {
            SelectedTab = existing;
            SelectedWorkspace = workspace;
            return existing;
        }

        var tab = new WorkspaceTabViewModel(workspace, documentId, title, documentNumber);
        Tabs.Add(tab);
        SelectedTab = tab;
        SelectedWorkspace = workspace;
        return tab;
    }

    /// <summary>Closes a tab without asking (hard close); closing the selected one selects its neighbour.</summary>
    public void CloseTab(WorkspaceTabViewModel tab)
    {
        ArgumentNullException.ThrowIfNull(tab);
        var index = Tabs.IndexOf(tab);
        if (index < 0)
        {
            return;
        }

        var wasSelected = ReferenceEquals(SelectedTab, tab);
        _suppressTabSync = true;
        try
        {
            Tabs.Remove(tab);
        }
        finally
        {
            _suppressTabSync = false;
        }

        TabClosed?.Invoke(this, new TabClosedEventArgs(tab, false));

        if (wasSelected)
        {
            SelectedTab = Tabs.Count == 0
                ? null
                : Tabs[Math.Min(index, Tabs.Count - 1)];
        }

        Launcher.Refresh();
        RaiseCommandsStateChanged();
    }

    /// <summary>Close through the command model: a dirty tab asks Save, Discard or Cancel.</summary>
    public async Task CloseTabInteractiveAsync(WorkspaceTabViewModel tab)
    {
        ArgumentNullException.ThrowIfNull(tab);
        if (tab.IsDirty && !await AskCloseTabAsync(tab))
        {
            return;
        }

        CloseTab(tab);
    }

    /// <summary>Close every unpinned tab except <paramref name="keep"/>; all decisions are
    /// gathered first and one Cancel keeps every tab (brief deliverable 4).</summary>
    public async Task CloseOtherTabsAsync(WorkspaceTabViewModel keep)
    {
        ArgumentNullException.ThrowIfNull(keep);
        var doomed = Tabs.Where(tab => !ReferenceEquals(tab, keep) && !tab.IsPinned).ToList();
        if (!await AskAllAsync(doomed))
        {
            return;
        }

        foreach (var tab in doomed)
        {
            CloseTab(tab);
        }

        SelectedTab = keep;
    }

    /// <summary>Close every unpinned tab; Cancel keeps all of them.</summary>
    public async Task CloseAllTabsAsync()
    {
        var doomed = Tabs.Where(tab => !tab.IsPinned).ToList();
        if (!await AskAllAsync(doomed))
        {
            return;
        }

        foreach (var tab in doomed)
        {
            CloseTab(tab);
        }
    }

    /// <summary>Ctrl+Tab / Ctrl+Shift+Tab: move to the next tab, wrapping around.</summary>
    public void CycleTab(bool forward)
    {
        if (Tabs.Count < 2 || SelectedTab is null)
        {
            return;
        }

        var index = Tabs.IndexOf(SelectedTab);
        var next = forward ? (index + 1) % Tabs.Count : (index - 1 + Tabs.Count) % Tabs.Count;
        SelectedTab = Tabs[next];
        SelectedWorkspace = SelectedTab.Workspace;
    }

    public void TogglePin(WorkspaceTabViewModel tab)
    {
        ArgumentNullException.ThrowIfNull(tab);
        tab.IsPinned = !tab.IsPinned;
        RaiseCommandsStateChanged();
    }

    /// <summary>Duplicate opens another tab of the same record (workspace tabs stay single).</summary>
    public void DuplicateTab(WorkspaceTabViewModel tab)
    {
        ArgumentNullException.ThrowIfNull(tab);
        if (tab.DocumentId is { } id)
        {
            var copy = new WorkspaceTabViewModel(tab.Workspace, Guid.NewGuid(), tab.Title + " (copy)", tab.DocumentNumber);
            Tabs.Add(copy);
            SelectedTab = copy;
            _ = id;
        }
    }

    /// <summary>Cycle expanded → icons-only → hidden → expanded.</summary>
    public void CycleSidebar()
    {
        Sidebar = Sidebar switch
        {
            SidebarMode.Expanded => SidebarMode.IconsOnly,
            SidebarMode.IconsOnly => SidebarMode.Hidden,
            _ => SidebarMode.Expanded,
        };
    }

    /// <summary>Open the Ctrl+K palette (the view saves the focus origin for the return).</summary>
    public void OpenLauncher(string? initialQuery = null)
    {
        Launcher.Query = initialQuery ?? string.Empty;
        Launcher.Refresh();
        IsLauncherOpen = true;
    }

    public void CloseLauncher() => IsLauncherOpen = false;

    /// <summary>Runs a command through the dispatcher with the current context; a refused
    /// run lands its reason on the status bar, so no trigger is ever inert.</summary>
    public async Task<DispatchResult> RunAsync(string id, CommandRoute route)
    {
        var result = await Dispatcher.ExecuteAsync(id, CurrentContext(), route);
        ReportDispatchResult(result);
        return result;
    }

    /// <summary>The status-bar hint when a dispatch was refused, so a greyed click still explains itself.</summary>
    internal void ReportDispatchResult(DispatchResult result)
    {
        if (!result.Executed && !result.Duplicate)
        {
            StatusMessage = result.Availability.Reason ?? "The command could not run.";
        }
    }

    /// <summary>Select a tab from the view (keeps the workspace highlight in sync).</summary>
    public void SelectTab(WorkspaceTabViewModel tab)
    {
        ArgumentNullException.ThrowIfNull(tab);
        SelectedTab = tab;
        SelectedWorkspace = tab.Workspace;
    }

    private async Task<bool> AskCloseTabAsync(WorkspaceTabViewModel tab)
    {
        if (tab.CloseGuard is { } guard)
        {
            // The tab's content owns the question (a form lists what would be lost and keeps the draft).
            return await guard();
        }

        if (ClosePromptRequested is null)
        {
            // No view to ask: treat as Cancel so an automated caller never silently drops
            // unsaved work.
            return false;
        }

        var choice = await ClosePromptRequested(tab);
        if (choice == TabCloseChoice.Save)
        {
            // The document save endpoint arrives with the form runtime (WP-UI-03+); the
            // chrome treats Save as "clean and close".
            tab.IsDirty = false;
            return true;
        }

        return choice == TabCloseChoice.Discard;
    }

    private async Task<bool> AskAllAsync(IReadOnlyList<WorkspaceTabViewModel> tabs)
    {
        var dirty = tabs.Where(tab => tab.IsDirty).ToList();
        var decisions = new List<(WorkspaceTabViewModel Tab, TabCloseChoice Choice)>(dirty.Count);
        foreach (var tab in dirty)
        {
            // A tab whose content owns its guard (a form) asks it; true means it may close.
            var choice = tab.CloseGuard is { } guard
                ? await guard() ? TabCloseChoice.Discard : TabCloseChoice.Cancel
                : await AskCloseTabOrCancelAsync(tab);
            decisions.Add((tab, choice));
        }

        if (decisions.Any(d => d.Choice == TabCloseChoice.Cancel))
        {
            return false;
        }

        foreach (var (tab, choice) in decisions.Where(d => d.Choice == TabCloseChoice.Save))
        {
            tab.IsDirty = false;
        }

        return true;
    }

    private async Task<TabCloseChoice> AskCloseTabOrCancelAsync(WorkspaceTabViewModel tab) =>
        ClosePromptRequested is null ? TabCloseChoice.Cancel : await ClosePromptRequested(tab);

    private void RaiseCommandsStateChanged() => CommandsStateChanged?.Invoke(this, EventArgs.Empty);

    private async Task LogoutAsync(object? parameter)
    {
        try
        {
            await _connection.LogoutAsync();
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            // Logout is idempotent and must always return the user to the login screen.
        }

        finally
        {
            FinishSignedOut();
            SignedOut?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>
    /// Called by the app when the API reports the session is gone (401
    /// <c>auth.session_expired</c> / <c>auth.invalid_credentials</c>): clear the shell and
    /// return to login. No server call — the token is already dropped.
    /// </summary>
    public void HandleSessionExpired()
    {
        FinishSignedOut();
        SignedOut?.Invoke(this, EventArgs.Empty);
    }

    private void FinishSignedOut()
    {
        IsLauncherOpen = false;
        foreach (var tab in Tabs.ToList())
        {
            TabClosed?.Invoke(this, new TabClosedEventArgs(tab, true));
        }

        Tabs.Clear();
        SelectedTab = null;
        UserName = string.Empty;
        UserEmail = string.Empty;
        UserId = string.Empty;
        SiteName = string.Empty;
        ExpiresAt = string.Empty;
        SessionError = null;
        SelectedWorkspace = "Home";
        NotificationCount = 0;
        StatusMessage = "Ready";
    }

    private bool _suppressTabSync;

    private void OnTabsChanged(object? sender, NotifyCollectionChangedEventArgs e)
    {
        CloseActiveTabCommand.RaiseCanExecuteChanged();
        if (_suppressTabSync)
        {
            return;
        }

        if (e.NewItems is { Count: > 0 } added && added[0] is WorkspaceTabViewModel first)
        {
            SelectedTab = first;
        }
        else if (Tabs.Count > 0 && SelectedTab is null)
        {
            SelectedTab = Tabs[0];
        }
    }
}
