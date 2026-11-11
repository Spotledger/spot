namespace Spotledger.Client.Designer;

/// <summary>What a canvas keyboard gesture does; each one is the keyboard route for a drag action.</summary>
public enum CanvasGesture
{
    /// <summary>Insert the chosen palette type at the selection (the keyboard route for a drop).</summary>
    Insert,

    /// <summary>Move the selected field one place up.</summary>
    MoveUp,

    /// <summary>Move the selected field one place down.</summary>
    MoveDown,

    /// <summary>Move the selected field into the next section.</summary>
    NextSection,

    /// <summary>Move the selected field into the previous section.</summary>
    PreviousSection,

    /// <summary>Make the selected section's span one column narrower.</summary>
    Narrower,

    /// <summary>Make the selected section's span one column wider.</summary>
    Wider,
}

/// <summary>
/// The keyboard map for the canvas: every drag action has a key gesture, so a designer who cannot
/// use a pointer can do the same work. Gesture names are dotnet-free strings (<c>Alt+Shift+Down</c>)
/// so the map is testable without a window and the view can build the name from its key event.
/// </summary>
public static class CanvasKeyboardMap
{
    /// <summary>The gesture catalogue, for the shortcut help.</summary>
    public static IReadOnlyList<(string Gesture, CanvasGesture Kind, string Description)> Bindings { get; } = new[]
    {
        ("Ctrl+I", CanvasGesture.Insert, "Insert the chosen field type"),
        ("Alt+Up", CanvasGesture.MoveUp, "Move the field up"),
        ("Alt+Down", CanvasGesture.MoveDown, "Move the field down"),
        ("Alt+Shift+Down", CanvasGesture.NextSection, "Move the field to the next section"),
        ("Alt+Shift+Up", CanvasGesture.PreviousSection, "Move the field to the previous section"),
        ("Alt+Left", CanvasGesture.Narrower, "Narrow the section by one column"),
        ("Alt+Right", CanvasGesture.Wider, "Widen the section by one column"),
    };

    /// <summary>
    /// Resolves a gesture name such as <c>alt+shift+down</c>, <c>Ctrl+I</c> or <c>Alt+ArrowUp</c>.
    /// Returns null for a name that is not a canvas gesture.
    /// </summary>
    public static CanvasGesture? Resolve(string? gesture)
    {
        if (string.IsNullOrWhiteSpace(gesture))
        {
            return null;
        }

        var parts = gesture.Split('+', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        var ctrl = false;
        var alt = false;
        var shift = false;
        string? key = null;
        foreach (var part in parts)
        {
            if (part.Equals("ctrl", StringComparison.OrdinalIgnoreCase) || part.Equals("control", StringComparison.OrdinalIgnoreCase))
            {
                ctrl = true;
            }
            else if (part.Equals("alt", StringComparison.OrdinalIgnoreCase))
            {
                alt = true;
            }
            else if (part.Equals("shift", StringComparison.OrdinalIgnoreCase))
            {
                shift = true;
            }
            else
            {
                key = NormalizeKey(part);
            }
        }

        if (key is null)
        {
            return null;
        }

        return (ctrl, alt, shift, key) switch
        {
            (true, false, false, "i") => CanvasGesture.Insert,
            (false, false, false, "insert") => CanvasGesture.Insert,
            (false, true, false, "up") => CanvasGesture.MoveUp,
            (false, true, false, "down") => CanvasGesture.MoveDown,
            (false, true, true, "down") => CanvasGesture.NextSection,
            (false, true, true, "up") => CanvasGesture.PreviousSection,
            (false, true, false, "left") => CanvasGesture.Narrower,
            (false, true, false, "right") => CanvasGesture.Wider,
            _ => null,
        };
    }

    /// <summary>The gesture name for a binding, for the view's key handler and the help text.</summary>
    public static string GestureText(CanvasGesture kind) =>
        Bindings.First(b => b.Kind == kind).Gesture;

    private static string NormalizeKey(string key)
    {
        var trimmed = key.Trim();
        if (trimmed.StartsWith("Arrow", StringComparison.OrdinalIgnoreCase) && trimmed.Length > 5)
        {
            trimmed = trimmed[5..];
        }

        return trimmed.ToLowerInvariant();
    }
}
