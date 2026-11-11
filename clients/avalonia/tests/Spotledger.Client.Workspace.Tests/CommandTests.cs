using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

public sealed class CommandTests
{
    private readonly ManualClock _clock = new();

    private static CommandRegistry Registry()
    {
        var registry = ShellCommands.BuildRegistry();
        WorkspaceCommands.Register(registry);
        return registry;
    }

    private UndoStack StackWithEdit(out DocumentDraft draft)
    {
        draft = Models.LoadedInvoice();
        var stack = new UndoStack(draft, _clock, schedule: _ => { });
        draft.Set("qty", new DecimalFieldValue(4m));
        return stack;
    }

    [Fact]
    public void Edit_menu_items_are_registered_through_the_shell_registry_without_collisions()
    {
        var registry = Registry();

        var items = registry.MenuItems("Edit");
        Assert.Contains(items, i => i.CommandId == "edit.undo" && i.Label == "Undo" && i.Mnemonic == 'U');
        Assert.Contains(items, i => i.CommandId == "edit.redo" && i.Label == "Redo" && i.Mnemonic == 'R');
        Assert.Contains(registry.MenuItems("File"), i => i.CommandId == "draft.recover" && i.Label == "Recover drafts");
        registry.ValidateMnemonics();
    }

    [Fact]
    public void The_keymaps_take_ctrl_z_ctrl_y_and_ctrl_shift_z_in_both_profiles()
    {
        var registry = Registry();
        foreach (var name in new[] { "desktop", "browser" })
        {
            var profile = WorkspaceCommands.Extend(KeymapProfile.Load(name), registry);
            Assert.Contains(profile.Bindings, b => b.Chord == "Ctrl+Z" && b.CommandId == "edit.undo");
            Assert.Contains(profile.Bindings, b => b.Chord == "Ctrl+Y" && b.CommandId == "edit.redo");
            Assert.Contains(profile.Bindings, b => b.Chord == "Ctrl+Shift+Z" && b.CommandId == "edit.redo");
        }
    }

    [Fact]
    public void Labels_name_the_next_action_and_availability_follows_the_history()
    {
        var provider = WorkspaceCommands.Availability(null, () => null);
        var registry = Registry();
        var undo = registry["edit.undo"];
        Assert.Equal("Undo", WorkspaceCommands.UndoText(null));
        Assert.False(provider(undo, new CommandContext()).IsAvailable);

        using var stack = StackWithEdit(out _);
        // The form's gate: this history's steps are header edits, so allowing every table
        // changes nothing here. A step that touches a table is refused without a gate.
        UndoTestGates.Allow(stack);
        provider = WorkspaceCommands.Availability(null, () => stack);
        Assert.True(provider(undo, new CommandContext()).IsAvailable);
        Assert.False(provider(registry["edit.redo"], new CommandContext()).IsAvailable);
        Assert.Equal("Undo Edit Quantity", WorkspaceCommands.UndoText(stack));
        Assert.Equal("Redo", WorkspaceCommands.RedoText(stack));

        _ = stack.Undo();
        Assert.False(provider(undo, new CommandContext()).IsAvailable);
        Assert.True(provider(registry["edit.redo"], new CommandContext()).IsAvailable);
        Assert.Equal("Undo", WorkspaceCommands.UndoText(stack));
        Assert.Equal("Redo Edit Quantity", WorkspaceCommands.RedoText(stack));
    }

    [Fact]
    public void While_a_text_box_has_the_focus_the_menu_items_are_blocked_with_a_reason()
    {
        using var stack = StackWithEdit(out _);
        var provider = WorkspaceCommands.Availability(null, () => stack);

        var answer = provider(Registry()["edit.undo"], new CommandContext(HasFocusedText: true));

        Assert.Equal(AvailabilityState.Blocked, answer.State);
        Assert.Contains("text box", answer.Reason);
    }

    [Fact]
    public void Other_commands_keep_the_inner_providers_answer()
    {
        var inner = new AvailabilityProvider((_, _) => CommandAvailability.Blocked("inner"));
        var provider = WorkspaceCommands.Availability(inner, () => null);
        Assert.Equal("inner", provider(Registry()["draft.recover"], new CommandContext()).Reason);
    }

    [Fact]
    public async Task The_dispatcher_runs_undo_and_redo_on_the_active_history()
    {
        var registry = Registry();
        var dispatcher = new CommandDispatcher(registry);
        using var stack = StackWithEdit(out var draft);
        dispatcher.Availability = WorkspaceCommands.Availability(null, () => stack);
        WorkspaceCommands.Bind(dispatcher, () => stack);

        var undo = await dispatcher.ExecuteAsync("edit.undo", new CommandContext(), CommandRoute.Shortcut);
        Assert.True(undo.Executed);
        Assert.Equal(1m, ((DecimalFieldValue)draft.Get("qty")!).ExactValue.ToDecimalValue());

        var again = await dispatcher.ExecuteAsync("edit.undo", new CommandContext(), CommandRoute.Menu);
        Assert.False(again.Executed);
        Assert.Equal("Nothing to undo.", again.Availability.Reason);

        _ = await dispatcher.ExecuteAsync("edit.redo", new CommandContext(), CommandRoute.Menu);
        Assert.Equal(4m, ((DecimalFieldValue)draft.Get("qty")!).ExactValue.ToDecimalValue());
    }

    [Theory]
    [InlineData(Key.Z, KeyModifiers.Control, false, UndoShortcut.Undo)]
    [InlineData(Key.Y, KeyModifiers.Control, false, UndoShortcut.Redo)]
    [InlineData(Key.Z, KeyModifiers.Control | KeyModifiers.Shift, false, UndoShortcut.Redo)]
    [InlineData(Key.Z, KeyModifiers.Control, true, UndoShortcut.None)]
    [InlineData(Key.Y, KeyModifiers.Control, true, UndoShortcut.None)]
    [InlineData(Key.Z, KeyModifiers.None, false, UndoShortcut.None)]
    [InlineData(Key.Z, KeyModifiers.Control | KeyModifiers.Alt, false, UndoShortcut.None)]
    [InlineData(Key.X, KeyModifiers.Control, false, UndoShortcut.None)]
    public void The_shortcut_table(Key key, KeyModifiers modifiers, bool inTextBox, UndoShortcut expected)
        => Assert.Equal(expected, UndoShortcuts.Decide(key, modifiers, inTextBox));

    [AvaloniaFact]
    public void Inside_a_text_box_ctrl_z_is_the_text_boxs_own_until_its_focus_ends()
    {
        using var stack = StackWithEdit(out var draft);
        var box = new TextBox { Text = "typing" };
        var other = new Button { Content = "Elsewhere" };
        var window = TestHost.Show(new StackPanel { Children = { box, other } });
        using var attached = UndoShortcuts.Attach(window, () => stack);

        _ = box.Focus();
        TestHost.Pump();
        window.KeyPress(Key.Z, RawInputModifiers.Control, PhysicalKey.Z, "z");
        TestHost.Pump();
        Assert.Equal(4m, ((DecimalFieldValue)draft.Get("qty")!).ExactValue.ToDecimalValue());
        Assert.True(UndoShortcuts.IsTextInput(box));

        _ = other.Focus();
        TestHost.Pump();
        window.KeyPress(Key.Z, RawInputModifiers.Control, PhysicalKey.Z, "z");
        TestHost.Pump();
        Assert.Equal(1m, ((DecimalFieldValue)draft.Get("qty")!).ExactValue.ToDecimalValue());

        window.KeyPress(Key.Y, RawInputModifiers.Control, PhysicalKey.Y, "y");
        TestHost.Pump();
        Assert.Equal(4m, ((DecimalFieldValue)draft.Get("qty")!).ExactValue.ToDecimalValue());
        window.Close();
    }
}
