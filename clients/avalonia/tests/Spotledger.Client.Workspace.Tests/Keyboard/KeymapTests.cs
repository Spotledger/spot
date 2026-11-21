using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Shell;
using Spotledger.Client.Workspace.Keyboard;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Keyboard;

/// <summary>
/// The keymap additions: they are registered through the shell's own registry and
/// <see cref="ShellViewModel.ExtendKeymaps"/> seam, so the shipped profile files are untouched. The
/// tests are the collision suite: no duplicate chord, no chord a text field owns, no chord a browser
/// swallows, and no chord that shadows a chrome command.
/// </summary>
public sealed class KeymapTests
{
    [AvaloniaFact]
    public void Every_addition_registers_against_the_shell_registry_without_a_collision()
    {
        foreach (var profile in new[] { "desktop", "browser" })
        {
            var registry = ShellCommands.BuildRegistry();
            SplitCommands.Register(registry);
            var extended = WorkspaceKeymaps.Load(profile);

            // Validate fails on a duplicate chord, an unknown command id or a browser-reserved chord.
            extended.Validate(registry);
            Assert.All(
                WorkspaceKeymaps.Additions,
                addition => Assert.Contains(addition, extended.Bindings));
        }
    }

    [AvaloniaFact]
    public void No_addition_is_a_chord_a_text_field_needs()
    {
        foreach (var addition in WorkspaceKeymaps.Additions)
        {
            Assert.False(
                WorkspaceKeymaps.IsTextEditingChord(addition.Chord),
                addition.Chord + " belongs to a text field and may not be shadowed by the workspace");
        }
    }

    [AvaloniaFact]
    public void No_addition_is_a_chord_a_browser_tab_swallows()
    {
        foreach (var addition in WorkspaceKeymaps.Additions)
        {
            Assert.DoesNotContain(
                KeymapProfile.BrowserReservedChords,
                reserved => string.Equals(
                    KeymapProfile.NormalizeChord(reserved),
                    KeymapProfile.NormalizeChord(addition.Chord),
                    StringComparison.OrdinalIgnoreCase));
        }
    }

    [AvaloniaFact]
    public void Both_profiles_bind_the_same_workspace_commands()
    {
        var desktop = WorkspaceKeymaps.Load("desktop");
        var browser = WorkspaceKeymaps.Load("browser");
        foreach (var id in SplitCommands.Ids)
        {
            Assert.NotNull(WorkspaceKeymaps.ChordFor(desktop, id));
            Assert.NotNull(WorkspaceKeymaps.ChordFor(browser, id));
        }

        // The tab additions reach the chrome's own commands instead of a second pair of ids.
        Assert.Equal("tab.next", WorkspaceKeymaps.CommandFor(desktop, "Ctrl+PageDown"));
        Assert.Equal("tab.previous", WorkspaceKeymaps.CommandFor(desktop, "Ctrl+PageUp"));
    }

    [AvaloniaFact]
    public void Extending_twice_does_not_duplicate_a_chord()
    {
        var once = WorkspaceKeymaps.Extend(KeymapProfile.Load("desktop"));
        var twice = WorkspaceKeymaps.Extend(once);

        Assert.Equal(once.Bindings.Count, twice.Bindings.Count);
        var registry = ShellCommands.BuildRegistry();
        SplitCommands.Register(registry);
        twice.Validate(registry);
    }

    [AvaloniaFact]
    public void Extending_refuses_to_shadow_a_chrome_chord()
    {
        // The desktop profile already binds Ctrl+Tab to tab.next: an addition on that chord must be
        // refused rather than silently win, which is what makes the collision suite meaningful.
        var desktop = KeymapProfile.Load("desktop");
        var shadowing = desktop with { Bindings = [.. desktop.Bindings, new Shell.KeyBinding("F6", "tab.next")] };

        var error = Assert.Throws<InvalidOperationException>(() => WorkspaceKeymaps.Extend(shadowing));
        Assert.Contains("F6", error.Message, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void The_split_commands_are_in_the_registry_the_menu_and_the_launcher_read()
    {
        var registry = ShellCommands.BuildRegistry();
        SplitCommands.Register(registry);

        Assert.All(SplitCommands.Ids, id => Assert.True(registry.TryGet(id, out _), id + " is registered"));
        Assert.Equal(SplitCommands.Ids.Length, SplitCommands.Definitions.Count);
    }
}
