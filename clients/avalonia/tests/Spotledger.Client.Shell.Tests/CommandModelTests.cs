using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The single command model: every route (menu, launcher, shortcut, context menu) reaches
/// the same handler with the same availability and reason; a running command ignores a
/// second trigger; keymaps are data validated for chord and mnemonic collisions in both
/// profiles (blueprint corrections §4).
/// </summary>
public sealed class CommandModelTests : IDisposable
{
    private readonly ApiConnection _connection;
    private readonly ShellViewModel _shell;

    public CommandModelTests()
    {
        _connection = new ApiConnection(new FakeServerHandler());
        _shell = new ShellViewModel(_connection);
    }

    public void Dispose() => _connection.Dispose();

    [Fact]
    public void TheShellRegistersTheNineTopLevelMenus()
    {
        var top = _shell.Registry.Branches
            .Where(branch => !branch.Path.Contains('/', StringComparison.Ordinal))
            .Select(branch => branch.Path)
            .ToArray();
        Assert.Equal(["File", "Edit", "View", "Transactions", "Processes", "Reports", "Tools", "Window", "Help"], top);
    }

    [Fact]
    public void TransactionsCarriesPurchasingSalesAndStockSubmenus()
    {
        var sub = _shell.Registry.MenuItems("Transactions").Select(item => item.Path).OfType<string>().ToArray();
        Assert.Equal(["Transactions/Purchasing", "Transactions/Sales", "Transactions/Stock"], sub);
        Assert.Equal(
            ["Purchase Request", "Supplier Quotation", "Purchase Order", "Goods Receipt", "Supplier Invoice", "Supplier Payment"],
            _shell.Registry.MenuItems("Transactions/Purchasing").Select(item => item.Label));
    }

    [Fact]
    public async Task EveryCommandIsEitherRunnableOrPermanentlyBlockedWithAReason()
    {
        // No stubs: a command with no handler must never report Available — the dispatcher
        // answers Blocked with a reason before it reaches the handler table.
        foreach (var definition in _shell.Registry.Definitions)
        {
            if (_shell.Dispatcher.HasHandler(definition.Id))
            {
                continue;
            }

            var result = await _shell.RunAsync(definition.Id, CommandRoute.Menu);
            Assert.False(result.Executed);
            Assert.False(result.Availability.IsAvailable);
            Assert.NotNull(result.Availability.Reason);
        }
    }

    [Theory]
    [InlineData("view.sidebar")]
    [InlineData("shell.launcher")]
    [InlineData("tab.new")]
    [InlineData("workspace.home")]
    [InlineData("create.purchaseOrder")]
    [InlineData("tab.close")]
    [InlineData("help.about")]
    public async Task AllRoutesShareTheHandlerAndTheReason(string id)
    {
        var viaMenu = await _shell.RunAsync(id, CommandRoute.Menu);
        var viaLauncher = await _shell.RunAsync(id, CommandRoute.Launcher);
        var viaShortcut = await _shell.RunAsync(id, CommandRoute.Shortcut);
        var viaContext = await _shell.RunAsync(id, CommandRoute.ContextMenu);
        foreach (var result in new[] { viaMenu, viaLauncher, viaShortcut, viaContext })
        {
            Assert.Equal(viaMenu.Executed, result.Executed);
            Assert.Equal(viaMenu.Availability.State, result.Availability.State);
            Assert.Equal(viaMenu.Availability.Reason, result.Availability.Reason);
        }

        // Same-handler proof: a route that ran really changed shell state.
        switch (id)
        {
            case "view.sidebar":
                Assert.NotEqual(SidebarMode.Expanded, _shell.Sidebar);
                break;
            case "shell.launcher":
                Assert.True(_shell.IsLauncherOpen);
                _shell.CloseLauncher();
                break;
            case "tab.new":
            case "workspace.home":
                Assert.Contains(_shell.Tabs, tab => tab.Workspace == "Home");
                break;
        }
    }

    [Fact]
    public async Task ABlockedCommandNeverRunsItsHandlerAndCarriesItsReason()
    {
        var result = await _shell.RunAsync("create.purchaseOrder", CommandRoute.Menu);
        Assert.False(result.Executed);
        Assert.Equal(AvailabilityState.Blocked, result.Availability.State);
        Assert.Contains("later task", result.Availability.Reason);
    }

    [Fact]
    public async Task TabCommandsAreBlockedWithAReasonWhileNoTabIsOpen()
    {
        Assert.Empty(_shell.Tabs);
        var result = await _shell.RunAsync("tab.close", CommandRoute.Menu);
        Assert.False(result.Executed);
        Assert.Equal("No tab is open.", result.Availability.Reason);
    }

    [Fact]
    public async Task EditCommandsNeedContextWhenNoTextFieldHasFocus()
    {
        _shell.IsFocusedText = false;
        var result = await _shell.RunAsync("edit.copy", CommandRoute.Menu);
        Assert.Equal(AvailabilityState.NeedsContext, result.Availability.State);
        Assert.Equal("Focus a text field first.", result.Availability.Reason);
    }

    [Fact]
    public async Task ACommandAlreadyRunningIgnoresTheSecondTrigger()
    {
        var registry = ShellCommands.BuildRegistry();
        var dispatcher = new CommandDispatcher(registry);
        var gate = new TaskCompletionSource();
        var runs = 0;
        dispatcher.Register("view.sidebar", async _ =>
        {
            Interlocked.Increment(ref runs);
            await gate.Task;
            Interlocked.Decrement(ref runs);
        });

        var first = dispatcher.ExecuteAsync("view.sidebar", new CommandContext(), CommandRoute.Menu);
        var second = await dispatcher.ExecuteAsync("view.sidebar", new CommandContext(), CommandRoute.Launcher);
        Assert.True(second.Duplicate);
        Assert.False(second.Executed);

        gate.SetResult();
        var firstResult = await first;
        Assert.True(firstResult.Executed);
        Assert.Equal(0, runs);
    }

    [Fact]
    public void MnemonicCollisionAtOneMenuLevelThrows()
    {
        var registry = new CommandRegistry();
        registry.AddBranch(new MenuBranch("File", "File", 'F'));
        registry.Add(new CommandDefinition("a", "Alpha", "d", "Open", "File", 'A'));
        var exception = Assert.Throws<InvalidOperationException>(
            () => registry.Add(new CommandDefinition("b", "Beta", "d", "Open", "File", 'A')));
        Assert.Contains("Mnemonic collision", exception.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void MnemonicsAreUniqueWithinEveryMenuLevelOfTheShell()
    {
        // The brief's examples: Transactions T vs Tools, Reports R vs Request-style ids.
        var registry = ShellCommands.BuildRegistry();
        registry.ValidateMnemonics();
        foreach (var level in registry.Branches.Select(b => b.Path).Prepend(""))
        {
            var letters = registry.MenuItems(level)
                .Where(item => item.Mnemonic != ' ')
                .Select(item => char.ToUpperInvariant(item.Mnemonic))
                .ToList();
            Assert.Equal(letters.Count, letters.Distinct().Count());
        }
    }

    [Fact]
    public void TheDesktopProfileReservesChordsWithoutCollision()
    {
        var desktop = KeymapProfile.Load("desktop");
        desktop.Validate(ShellCommands.BuildRegistry());
        Assert.NotNull(desktop.Find("ctrl+k"));
        Assert.Equal("shell.launcher", desktop.Find("Ctrl+K")!.CommandId);
        Assert.Equal("tab.close", desktop.Find("Ctrl+W")!.CommandId);
    }

    [Fact]
    public void TheBrowserProfileAvoidsChordsTheBrowserOwns()
    {
        var browser = KeymapProfile.Load("browser");
        browser.Validate(ShellCommands.BuildRegistry());
        Assert.DoesNotContain(browser.Bindings, binding =>
            KeymapProfile.BrowserReservedChords.Any(reserved =>
                string.Equals(KeymapProfile.NormalizeChord(reserved),
                              KeymapProfile.NormalizeChord(binding.Chord),
                              StringComparison.OrdinalIgnoreCase)));
        Assert.Equal("tab.close", browser.Find("Ctrl+Shift+F4")!.CommandId);
    }

    [Fact]
    public void TwoCommandsSharingAChordFailValidation()
    {
        var registry = ShellCommands.BuildRegistry();
        var colliding = new KeymapProfile("desktop", true,
            [new KeyBinding("Ctrl+K", "shell.launcher"), new KeyBinding("Ctrl+Shift+K", "shell.new"), new KeyBinding("Ctrl+K", "shell.new")]);
        var exception = Assert.Throws<InvalidOperationException>(() => colliding.Validate(registry));
        Assert.Contains("Duplicate chord", exception.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AProfileBindingToAnUnknownCommandFailsValidation()
    {
        var registry = ShellCommands.BuildRegistry();
        var bad = new KeymapProfile("desktop", true, [new KeyBinding("Ctrl+J", "no.such.command")]);
        Assert.Throws<KeyNotFoundException>(() => bad.Validate(registry));
    }

    [Fact]
    public void ChordsParseToAvaloniaKeysAndModifiers()
    {
        Assert.Equal((Avalonia.Input.Key.K, Avalonia.Input.KeyModifiers.Control),
            KeymapProfile.ParseChord("Ctrl+K"));
        Assert.Equal((Avalonia.Input.Key.Tab, Avalonia.Input.KeyModifiers.Control | Avalonia.Input.KeyModifiers.Shift),
            KeymapProfile.ParseChord("Ctrl+Shift+Tab"));
        Assert.Equal((Avalonia.Input.Key.F10, Avalonia.Input.KeyModifiers.None),
            KeymapProfile.ParseChord("F10"));
    }
}
