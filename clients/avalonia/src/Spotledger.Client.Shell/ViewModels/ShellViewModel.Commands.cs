namespace Spotledger.Client.Shell;

/// <summary>
/// The chrome wiring half of <see cref="ShellViewModel"/>: every command id gets one
/// handler here, and menu, launcher, shortcut and tab-menu triggers all arrive through
/// the dispatcher, so all four routes reach the same handler with the same reason.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>Raised when F10 (window.menu) asks the view to focus the menu bar.</summary>
    public event EventHandler? MenuBarFocusRequested;

    /// <summary>Builds one bound command object per registry entry (id → ShellCommand).</summary>
    private void BuildShellCommands()
    {
        var map = new Dictionary<string, ShellCommand>(StringComparer.Ordinal);
        foreach (var definition in Registry.Definitions)
        {
            map[definition.Id] = new ShellCommand(definition, this);
        }

        Commands = map;
    }

    /// <summary>Switches the active keyboard profile; validates against the registry first.</summary>
    public void SetKeymapProfile(string name)
    {
        var profile = KeymapProfile.Load(name);
        if (_keymapExtender is { } extend)
        {
            profile = extend(profile);
        }

        profile.Validate(Registry);
        Keymap = profile;
        OnPropertyChanged(nameof(Keymap));
        foreach (var command in Commands.Values)
        {
            command.NotifyGestureChanged();
        }
    }

    private void RegisterChromeHandlers()
    {
        void Register(string id, Func<CommandContext, Task> handler) => Dispatcher.Register(id, handler);
        Task Done() => Task.CompletedTask;

        Register("shell.launcher", _ => { OpenLauncher(); return Done(); });
        Register("shell.new", _ => { OpenLauncher(); return Done(); });
        Register("shell.logout", _ => LogoutAsync(null));
        Register("shell.changePassword", _ => OpenChangePasswordAsync());
        Register("shell.mySessions", _ => OpenMySessionsAsync());
        Register("shell.signinAgain", _ => { RequestSignInAgain(); return Done(); });
        Register("window.menu", _ => { MenuBarFocusRequested?.Invoke(this, EventArgs.Empty); return Done(); });
        Register("view.sidebar", _ => { CycleSidebar(); return Done(); });

        foreach (var (commandId, label) in ShellCommands.Workspaces)
        {
            var workspace = label;
            Register(commandId, _ => OpenTabAsync(workspace));
        }

        Register("process.myTasks", _ => OpenTabAsync("My Tasks"));

        Register("tab.new", _ => OpenTabAsync("Home"));
        Register("tab.next", _ => { CycleTab(forward: true); return Done(); });
        Register("tab.previous", _ => { CycleTab(forward: false); return Done(); });
        Register("tab.close", ctx => ctx.ActiveTab is { } tab
            ? CloseTabInteractiveAsync(tab)
            : Done());
        Register("tab.others", ctx => ctx.ActiveTab is { } keep
            ? CloseOtherTabsAsync(keep)
            : Done());
        Register("tab.all", _ => CloseAllTabsAsync());
        Register("tab.pin", ctx =>
        {
            if (ctx.ActiveTab is { } tab)
            {
                TogglePin(tab);
            }

            return Done();
        });
        Register("tab.duplicate", ctx =>
        {
            if (ctx.ActiveTab is { } tab)
            {
                DuplicateTab(tab);
            }

            return Done();
        });
        Register("tab.focus", ctx =>
        {
            var target = Tabs.FirstOrDefault(tab => string.Equals(tab.Key, ctx.FocusTabKey, StringComparison.Ordinal));
            if (target is not null)
            {
                SelectedTab = target;
                SelectedWorkspace = target.Workspace;
            }

            IsLauncherOpen = false;
            return Done();
        });

        foreach (var action in new[] { "cut", "copy", "paste", "selectAll" })
        {
            var name = action;
            Register("edit." + action, _ =>
            {
                ClipboardActionRequested?.Invoke(name);
                return Done();
            });
        }

        foreach (var id in new[] { "tools.keymap", "help.keymap" })
        {
            Register(id, _ =>
            {
                OpenLauncher();
                Launcher.ShowKeymap(Keymap);
                return Done();
            });
        }

        Register("tools.density", _ => { CompactDensity = !CompactDensity; return Done(); });
        Register("tools.locale", _ => { RightToLeft = !RightToLeft; return Done(); });
        Register("help.about", _ =>
        {
            StatusMessage = $"Spotledger client · session {SiteName} · expires {ExpiresAt}";
            return Done();
        });
    }
}
