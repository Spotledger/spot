using System.Globalization;
using Avalonia.Input;

namespace Spotledger.Client.Shell;

/// <summary>One JSON keymap entry: a chord string and the command id it runs.</summary>
public sealed record KeyBinding(string Chord, string CommandId);

/// <summary>
/// A keyboard profile shipped as data, not hard-coded (blueprint corrections §4): the
/// <c>desktop</c> profile owns Alt menus and Ctrl+W; the <c>browser</c> profile avoids
/// chords a browser cannot capture.
/// </summary>
public sealed record KeymapProfile(string Name, bool AltMenus, IReadOnlyList<KeyBinding> Bindings)
{
    private static readonly System.Text.Json.JsonSerializerOptions JsonOptions =
        new() { PropertyNameCaseInsensitive = true };

    /// <summary>Chords a browser tab can never hand to the app.</summary>
    public static readonly IReadOnlyList<string> BrowserReservedChords = ["Ctrl+W", "Ctrl+T", "Ctrl+N"];

    public static KeymapProfile Load(string name)
    {
        var stream = typeof(KeymapProfile).Assembly.GetManifestResourceStream($"Spotledger.Client.Shell.Keymaps.{name}.json")
            ?? throw new InvalidOperationException($"Missing keymap profile '{name}'.");
        using (stream)
        {
            var profile = System.Text.Json.JsonSerializer.Deserialize<KeymapProfile>(stream, JsonOptions)
                ?? throw new InvalidOperationException($"Keymap profile '{name}' is not valid JSON.");
            if (!string.Equals(profile.Name, name, StringComparison.Ordinal))
            {
                throw new InvalidOperationException($"Keymap file for '{name}' declares profile '{profile.Name}'.");
            }

            return profile;
        }
    }

    /// <summary>Fails on a duplicate chord, an unknown command id, or a reserved browser chord.</summary>
    public void Validate(CommandRegistry registry)
    {
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var binding in Bindings)
        {
            var normalized = NormalizeChord(binding.Chord);
            if (!seen.Add(normalized))
            {
                throw new InvalidOperationException($"Duplicate chord '{binding.Chord}' in keymap '{Name}'.");
            }

            _ = registry[binding.CommandId];
            if (Name == "browser"
                && BrowserReservedChords.Any(reserved => string.Equals(NormalizeChord(reserved), normalized, StringComparison.OrdinalIgnoreCase)))
            {
                throw new InvalidOperationException($"Chord '{binding.Chord}' cannot be captured in the browser profile.");
            }

            // Parse every chord now: an unparseable chord would silently never fire.
            ParseChord(binding.Chord);
        }
    }

    public KeyBinding? Find(string chord)
    {
        var normalized = NormalizeChord(chord);
        return Bindings.FirstOrDefault(binding => string.Equals(NormalizeChord(binding.Chord), normalized, StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>True when the key event matches the chord text (e.g. "Ctrl+Shift+Tab").</summary>
    public static bool Matches(string chord, Key key, KeyModifiers modifiers)
    {
        var (chordKey, chordModifiers) = ParseChord(chord);
        return key == chordKey && modifiers == chordModifiers;
    }

    /// <summary>Turns "Ctrl+Shift+Tab" into an Avalonia key and modifier set.</summary>
    public static (Key Key, KeyModifiers Modifiers) ParseChord(string chord)
    {
        var modifiers = KeyModifiers.None;
        Key? key = null;
        foreach (var part in chord.Split('+'))
        {
            var token = part.Trim();
            switch (token.ToLowerInvariant())
            {
                case "ctrl":
                    modifiers |= KeyModifiers.Control;
                    break;
                case "shift":
                    modifiers |= KeyModifiers.Shift;
                    break;
                case "alt":
                    modifiers |= KeyModifiers.Alt;
                    break;
                case "win" or "super" or "cmd":
                    modifiers |= KeyModifiers.Meta;
                    break;
                default:
                    if (key is not null)
                    {
                        throw new FormatException($"Chord '{chord}' has more than one key.");
                    }

                    key = ParseKey(token);
                    break;
            }
        }

        return key.HasValue
            ? (key.Value, modifiers)
            : throw new FormatException($"Chord '{chord}' has no key.");
    }

    private static Key ParseKey(string token)
    {
        if (token.Length == 1 && (token[0] >= 'A' && token[0] <= 'Z' || token[0] >= 'a' && token[0] <= 'z'))
        {
            return Key.A + (char.ToUpperInvariant(token[0]) - 'A');
        }

        if (token.Length == 1 && char.IsDigit(token[0]))
        {
            return Key.D0 + (token[0] - '0');
        }

        if (token.Length == 1)
        {
            return token[0] switch
            {
                ' ' => Key.Space,
                '-' => Key.OemMinus,
                '=' => Key.OemPlus,
                _ => throw new FormatException($"Single-character key '{token}' is not supported."),
            };
        }

        // Special names (Tab, Escape, F1..) map onto Avalonia's enum via invariant case.
        var candidate = token switch
        {
            _ when token.Equals("escape", StringComparison.OrdinalIgnoreCase) => "Escape",
            _ when token.Equals("enter", StringComparison.OrdinalIgnoreCase) => "Enter",
            _ when token.Equals("return", StringComparison.OrdinalIgnoreCase) => "Enter",
            _ when token.Equals("space", StringComparison.OrdinalIgnoreCase) => "Space",
            _ => token,
        };
        return Enum.TryParse(candidate, ignoreCase: true, out Key parsed)
            ? parsed
            : throw new FormatException($"Unknown key '{token}' in a chord.");
    }

    /// <summary>Canonical "Ctrl+Shift+Tab" spelling for duplicate detection.</summary>
    public static string NormalizeChord(string chord)
    {
        var parts = chord.Split('+').Select(part => part.Trim()).Where(part => part.Length > 0).ToList();
        var ordered = new List<string>();
        foreach (var name in new[] { "ctrl", "alt", "shift", "win" })
        {
            var match = parts.FirstOrDefault(part => string.Equals(part, name, StringComparison.OrdinalIgnoreCase));
            if (match is not null)
            {
                ordered.Add(match.ToLowerInvariant());
            }
        }

        foreach (var part in parts.Where(part => !ordered.Contains(part.ToLowerInvariant())))
        {
            ordered.Add(part.ToUpperInvariant());
        }

        return string.Join('+', ordered);
    }
}
