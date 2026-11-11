using Avalonia.Controls;
using Avalonia.Input;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Workspace;

/// <summary>What a keystroke means to the undo history.</summary>
public enum UndoShortcut
{
    /// <summary>Not an undo or redo key, or a text box owns it.</summary>
    None,

    /// <summary>Ctrl+Z.</summary>
    Undo,

    /// <summary>Ctrl+Y or Ctrl+Shift+Z.</summary>
    Redo,
}

/// <summary>
/// Ctrl+Z, Ctrl+Y and Ctrl+Shift+Z, and the one rule that goes with them: while a text box has the
/// focus, those keys are the text box's own (its character-level undo), and they belong to the form's
/// history again once the focus leaves, when the box has committed its value to the draft.
/// </summary>
public static class UndoShortcuts
{
    /// <summary>Decides what a key does.</summary>
    /// <param name="key">The key.</param>
    /// <param name="modifiers">The modifiers held.</param>
    /// <param name="textInputFocused">True when a text box has the keyboard focus.</param>
    public static UndoShortcut Decide(Key key, KeyModifiers modifiers, bool textInputFocused)
    {
        if (textInputFocused || !modifiers.HasFlag(KeyModifiers.Control) || modifiers.HasFlag(KeyModifiers.Alt))
        {
            return UndoShortcut.None;
        }

        var shift = modifiers.HasFlag(KeyModifiers.Shift);
        return key switch
        {
            Key.Z when !shift => UndoShortcut.Undo,
            Key.Z => UndoShortcut.Redo,
            Key.Y when !shift => UndoShortcut.Redo,
            _ => UndoShortcut.None,
        };
    }

    /// <summary>True when the focused element takes typed text itself.</summary>
    public static bool IsTextInput(object? focused) => focused is TextBox;

    /// <summary>
    /// Hooks the shortcuts on a window or view: a tunnelling key handler that asks the active history
    /// to undo or redo unless a text box is focused. Returns the unsubscribe.
    /// </summary>
    public static IDisposable Attach(InputElement root, Func<UndoStack?> active)
    {
        ArgumentNullException.ThrowIfNull(root);
        ArgumentNullException.ThrowIfNull(active);
        void OnKeyDown(object? sender, KeyEventArgs e)
        {
            var focused = TopLevel.GetTopLevel(root)?.FocusManager?.GetFocusedElement();
            var action = Decide(e.Key, e.KeyModifiers, IsTextInput(focused));
            var stack = active();
            if (action == UndoShortcut.None || stack is null)
            {
                return;
            }

            e.Handled = action == UndoShortcut.Undo ? stack.Undo() : stack.Redo();
        }

        root.AddHandler(InputElement.KeyDownEvent, OnKeyDown, Avalonia.Interactivity.RoutingStrategies.Tunnel);
        return new Detach(() => root.RemoveHandler(InputElement.KeyDownEvent, OnKeyDown));
    }

    private sealed class Detach(Action action) : IDisposable
    {
        public void Dispose() => action();
    }
}

/// <summary>
/// Registers the workspace's commands in the shell's one command registry: Edit &gt; Undo, Edit &gt;
/// Redo (their menu text carries the next action: "Undo Edit Quantity") and File &gt; Recover drafts.
/// Nothing in the Shell is edited: the definitions are added to the registry the shell built, the
/// chords are added to a copy of a keymap profile, and the availability and handlers are wrapped around
/// the dispatcher's.
/// </summary>
public static class WorkspaceCommands
{
    /// <summary>The id of Edit &gt; Undo.</summary>
    public const string UndoId = "edit.undo";

    /// <summary>The id of Edit &gt; Redo.</summary>
    public const string RedoId = "edit.redo";

    /// <summary>The id of File &gt; Recover drafts.</summary>
    public const string RecoverDraftsId = "draft.recover";

    /// <summary>The chords the workspace adds to a keymap profile.</summary>
    public static readonly IReadOnlyList<Spotledger.Client.Shell.KeyBinding> Bindings =
    [
        new("Ctrl+Z", UndoId),
        new("Ctrl+Y", RedoId),
        new("Ctrl+Shift+Z", RedoId),
    ];

    /// <summary>Adds the three definitions to the registry.</summary>
    public static void Register(CommandRegistry registry)
    {
        ArgumentNullException.ThrowIfNull(registry);
        registry.Add(new CommandDefinition(UndoId, "Undo", "Undo the last edit in this form", "Open", "Edit", 'U') { SearchTerms = ["undo", "revert"] });
        registry.Add(new CommandDefinition(RedoId, "Redo", "Redo the edit that was undone", "Open", "Edit", 'R') { SearchTerms = ["redo"] });
        registry.Add(new CommandDefinition(RecoverDraftsId, "Recover drafts", "Open unsaved changes kept from an earlier session", "Open", "File", 'D')
        {
            SearchTerms = ["recover", "draft", "unsaved", "restore"],
        });
    }

    /// <summary>A copy of the keymap profile with the undo and redo chords added (and validated against the registry).</summary>
    public static KeymapProfile Extend(KeymapProfile profile, CommandRegistry registry)
    {
        ArgumentNullException.ThrowIfNull(profile);
        var extended = profile with { Bindings = [.. profile.Bindings, .. Bindings] };
        extended.Validate(registry);
        return extended;
    }

    /// <summary>The menu text of Undo: "Undo Edit Quantity", or "Undo" when there is nothing to undo.</summary>
    public static string UndoText(UndoStack? stack) => stack?.UndoLabel is { } label ? "Undo " + label : "Undo";

    /// <summary>The menu text of Redo.</summary>
    public static string RedoText(UndoStack? stack) => stack?.RedoLabel is { } label ? "Redo " + label : "Redo";

    /// <summary>
    /// Wraps an availability provider: Undo and Redo are available only when the active form has a step
    /// to reverse or replay, and never while a text box has the focus (the text box's own undo applies).
    /// Every other command is the inner provider's answer.
    /// </summary>
    public static AvailabilityProvider Availability(AvailabilityProvider? inner, Func<UndoStack?> active)
    {
        ArgumentNullException.ThrowIfNull(active);
        return (definition, context) =>
        {
            if (definition.Id is UndoId or RedoId)
            {
                var undo = definition.Id == UndoId;
                if (context.HasFocusedText)
                {
                    return CommandAvailability.Blocked("A text box has the focus: undo works inside the box until you leave it.");
                }

                var stack = active();
                if (stack is null)
                {
                    return CommandAvailability.Blocked("No form is open.");
                }

                return (undo ? stack.CanUndo : stack.CanRedo)
                    ? CommandAvailability.Available
                    : CommandAvailability.Blocked(undo ? "Nothing to undo." : "Nothing to redo.");
            }

            return inner?.Invoke(definition, context) ?? CommandAvailability.Available;
        };
    }

    /// <summary>Registers the Undo and Redo handlers on the dispatcher, acting on the active form's history.</summary>
    public static void Bind(CommandDispatcher dispatcher, Func<UndoStack?> active)
    {
        ArgumentNullException.ThrowIfNull(dispatcher);
        ArgumentNullException.ThrowIfNull(active);
        dispatcher.Register(UndoId, context =>
        {
            _ = active()?.Undo();
            return Task.CompletedTask;
        });
        dispatcher.Register(RedoId, context =>
        {
            _ = active()?.Redo();
            return Task.CompletedTask;
        });
    }
}
