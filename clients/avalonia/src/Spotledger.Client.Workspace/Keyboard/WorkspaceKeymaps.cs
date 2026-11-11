using Avalonia.Input;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Workspace.Keyboard;

/// <summary>
/// The chords the keyboard-first workspace adds, registered at runtime through the shell's
/// <see cref="ShellViewModel.ExtendKeymaps"/> seam (the shipped profile JSON files are generated
/// artifacts of the chrome and are never edited here). Both profiles get the same additions: none of
/// them is a chord a browser tab swallows, and none of them is a chord a text field needs.
/// <para>See <c>FOCUS.md</c> for the table and for why each chord was chosen.</para>
/// </summary>
public static class WorkspaceKeymaps
{
    /// <summary>Chords a text field owns; a workspace chord may never shadow one.</summary>
    public static readonly IReadOnlyList<string> TextEditingChords =
    [
        "Left", "Right", "Up", "Down", "Home", "End", "PageUp", "PageDown", "Delete", "Backspace", "Enter", "Tab",
        "Shift+Tab", "Ctrl+Left", "Ctrl+Right", "Ctrl+Home", "Ctrl+End", "Ctrl+A", "Ctrl+C", "Ctrl+V", "Ctrl+X",
        "Ctrl+Z", "Ctrl+Y", "Ctrl+S", "Ctrl+F", "Ctrl+Shift+Z",
    ];

    /// <summary>The additions, in registration order; a profile gets every one of them.</summary>
    public static IReadOnlyList<Shell.KeyBinding> Additions { get; } =
    [
        // Modes: Ctrl+Alt+digit, the GNOME/Windows convention for a view switch that must not fire
        // while typing (Alt alone would open menus).
        new("Ctrl+Alt+1", SplitCommands.ModeList),
        new("Ctrl+Alt+2", SplitCommands.ModeSplit),
        new("Ctrl+Alt+3", SplitCommands.ModeForm),

        // Walking a worklist: Alt+Down/Up is the Windows convention for "next/previous record" and no
        // text box consumes it.
        new("Alt+Down", SplitCommands.WalkNext),
        new("Alt+Up", SplitCommands.WalkPrevious),

        // Pane focus. F6/Shift+F6 cycle every pane of the window; the two direct chords jump.
        new("F6", SplitCommands.PaneNext),
        new("Shift+F6", SplitCommands.PanePrevious),
        new("Ctrl+Alt+L", SplitCommands.FocusList),
        new("Ctrl+Alt+F", SplitCommands.FocusForm),

        // The document commands of the arrangement.
        new("Ctrl+Enter", SplitCommands.OpenInTab),
        new("Ctrl+Alt+N", SplitCommands.NewFromList),
        new("Ctrl+Alt+S", SplitCommands.SaveAndNext),

        // Tab movement; Ctrl+Tab is already the chrome's, so the page keys are added for the same two
        // commands rather than for new ones (a second id would split the same behaviour in two).
        new("Ctrl+PageDown", "tab.next"),
        new("Ctrl+PageUp", "tab.previous"),
    ];

    /// <summary>
    /// Returns <paramref name="profile"/> with the additions appended. Applying the extension twice is
    /// safe (a chord already bound to the same command is kept once); a chord already bound to another
    /// command is refused, because silently shadowing a chrome shortcut is exactly the bug the
    /// collision tests exist to catch.
    /// </summary>
    public static KeymapProfile Extend(KeymapProfile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        var bindings = profile.Bindings.ToList();
        foreach (var addition in Additions)
        {
            var existing = bindings.FirstOrDefault(binding =>
                string.Equals(KeymapProfile.NormalizeChord(binding.Chord), KeymapProfile.NormalizeChord(addition.Chord), StringComparison.OrdinalIgnoreCase));
            if (existing is not null)
            {
                if (!string.Equals(existing.CommandId, addition.CommandId, StringComparison.Ordinal))
                {
                    throw new InvalidOperationException(
                        $"Chord '{addition.Chord}' of profile '{profile.Name}' already runs '{existing.CommandId}': the workspace may not shadow it.");
                }

                continue;
            }

            bindings.Add(addition);
        }

        return profile with { Bindings = bindings };
    }

    /// <summary>Loads a shipped profile and applies the additions, as the shell does at startup.</summary>
    public static KeymapProfile Load(string name) => Extend(KeymapProfile.Load(name));

    /// <summary>The command a chord runs in an extended profile, or null.</summary>
    public static string? CommandFor(KeymapProfile profile, string chord)
    {
        ArgumentNullException.ThrowIfNull(profile);
        return profile.Find(chord)?.CommandId;
    }

    /// <summary>The chord an extended profile binds to <paramref name="commandId"/>, or null.</summary>
    public static string? ChordFor(KeymapProfile profile, string commandId)
    {
        ArgumentNullException.ThrowIfNull(profile);
        return profile.Bindings.FirstOrDefault(binding => string.Equals(binding.CommandId, commandId, StringComparison.Ordinal))?.Chord;
    }

    /// <summary>True when a chord belongs to a text field (the workspace must never use one).</summary>
    public static bool IsTextEditingChord(string chord)
        => TextEditingChords.Any(owned =>
            string.Equals(KeymapProfile.NormalizeChord(owned), KeymapProfile.NormalizeChord(chord), StringComparison.OrdinalIgnoreCase));

    /// <summary>The command of the key event in <paramref name="profile"/>, or null.</summary>
    public static string? CommandOf(KeymapProfile profile, Key key, KeyModifiers modifiers)
    {
        ArgumentNullException.ThrowIfNull(profile);
        foreach (var binding in profile.Bindings)
        {
            if (KeymapProfile.Matches(binding.Chord, key, modifiers))
            {
                return binding.CommandId;
            }
        }

        return null;
    }
}
