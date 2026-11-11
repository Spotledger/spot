namespace Spotledger.Client.Shell;

/// <summary>
/// The seams a host (the desktop app) uses to add what the chrome does not know about: commands, key
/// chords, a command label that follows state ("Undo Edit Quantity"), tabs built by a tab kind, and the
/// availability rules of those commands (WP-UI-17.T4). Everything goes through the one registry, the one
/// dispatcher and the one keymap, so menu, launcher and shortcut stay the same command.
/// </summary>
public sealed partial class ShellViewModel
{
    private Func<KeymapProfile, KeymapProfile>? _keymapExtender;

    /// <summary>Gives a command its label when it differs from the registered one (null keeps it).</summary>
    public Func<CommandDefinition, string?>? CommandLabelProvider { get; set; }

    /// <summary>Raised after a tab has left <see cref="Tabs"/> (closed by the user or by sign-out).</summary>
    public event EventHandler<TabClosedEventArgs>? TabClosed;

    internal string LabelOf(CommandDefinition definition)
        => CommandLabelProvider?.Invoke(definition) ?? definition.Label;

    /// <summary>Adds commands to the registry the shell built and makes them part of the menu, the
    /// launcher and the dispatcher. Call before the shell view is built (the menu is built from the registry).</summary>
    public void AddCommands(Action<CommandRegistry> register)
    {
        ArgumentNullException.ThrowIfNull(register);
        register(Registry);
        BuildShellCommands();
    }

    /// <summary>Wraps the availability rules: the wrapper receives the shell's own provider and returns the new one.</summary>
    public void WrapAvailability(Func<AvailabilityProvider?, AvailabilityProvider> wrap)
    {
        ArgumentNullException.ThrowIfNull(wrap);
        Dispatcher.Availability = wrap(Dispatcher.Availability);
        RaiseCommandsStateChanged();
    }

    /// <summary>Adds chords to every keymap profile: the extender receives a profile and returns it
    /// with the host's chords (validated against the registry). Applies to the profile in use and to
    /// each profile loaded later.</summary>
    public void ExtendKeymaps(Func<KeymapProfile, KeymapProfile> extend)
    {
        ArgumentNullException.ThrowIfNull(extend);
        _keymapExtender = extend;
        var wasDesktop = ReferenceEquals(Keymap, DesktopKeymap);
        DesktopKeymap = extend(DesktopKeymap);
        Keymap = wasDesktop ? DesktopKeymap : extend(Keymap);
        OnPropertyChanged(nameof(Keymap));
        foreach (var command in Commands.Values)
        {
            command.NotifyGestureChanged();
        }
    }

    /// <summary>Rebuilds the menu bar and the status bar (a command label changed).</summary>
    public void RefreshMenu() => OnPropertyChanged(nameof(Keymap));

    /// <summary>Tells menu items and the launcher to ask the dispatcher about availability again.</summary>
    public void RefreshCommandState() => RaiseCommandsStateChanged();

    /// <summary>Puts a quiet line on the status bar.</summary>
    public void ShowStatus(string message)
    {
        ArgumentNullException.ThrowIfNull(message);
        StatusMessage = message;
    }

    /// <summary>
    /// Opens a tab whose content a host built (a document form, the recover-drafts list). A tab whose
    /// key is already open is selected instead of opened twice.
    /// </summary>
    public WorkspaceTabViewModel OpenContentTab(string title, object content, string key, string workspace = "Home")
    {
        ArgumentNullException.ThrowIfNull(title);
        ArgumentNullException.ThrowIfNull(content);
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        var existing = Tabs.FirstOrDefault(tab => string.Equals(tab.Key, key, StringComparison.Ordinal));
        if (existing is not null)
        {
            SelectTab(existing);
            return existing;
        }

        var tab = new WorkspaceTabViewModel(workspace, key, title, content);
        Tabs.Add(tab);
        SelectTab(tab);
        Launcher.Refresh();
        RaiseCommandsStateChanged();
        return tab;
    }
}

/// <summary>A tab that has left the shell.</summary>
/// <param name="Tab">The tab.</param>
/// <param name="SignedOut">True when the whole session ended (sign-out or expiry): the host must not write anything for it.</param>
public sealed class TabClosedEventArgs(WorkspaceTabViewModel tab, bool signedOut) : EventArgs
{
    /// <summary>The tab.</summary>
    public WorkspaceTabViewModel Tab { get; } = tab;

    /// <summary>True when the session ended.</summary>
    public bool SignedOut { get; } = signedOut;
}
