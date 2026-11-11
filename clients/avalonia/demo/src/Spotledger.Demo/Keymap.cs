using System.Reflection;
using System.Text;
using System.Text.Json;

namespace Spotledger.Demo;

public sealed record KeyBinding(string Chord, string CommandId);
public sealed record KeymapProfile(string Name, bool AltMenus, IReadOnlyList<KeyBinding> Bindings)
{
    private static readonly JsonSerializerOptions JsonOptions = new() { PropertyNameCaseInsensitive = true };
    public static KeymapProfile Load(string name)
    {
        using var stream = Assembly.GetExecutingAssembly().GetManifestResourceStream($"Spotledger.Demo.Keymaps.{name}.json")
            ?? throw new InvalidOperationException($"Missing keymap {name}");
        return JsonSerializer.Deserialize<KeymapProfile>(stream, JsonOptions)!;
    }
    public void Validate(CommandRegistry registry)
    {
        if (Bindings.Select(b => b.Chord.ToUpperInvariant()).Distinct(StringComparer.Ordinal).Count() != Bindings.Count)
            throw new InvalidOperationException($"Duplicate chord in {Name}");
        foreach (var binding in Bindings)
        {
            _ = registry[binding.CommandId];
            if (Name == "browser" && new[] { "Ctrl+W", "Ctrl+T", "Ctrl+N" }.Contains(binding.Chord, StringComparer.OrdinalIgnoreCase))
                throw new InvalidOperationException("Browser reserved chord");
        }
        foreach (var path in CommandRegistry.Branches.Select(b => b.Path).Prepend(""))
        {
            var items = registry.MenuItems(path);
            if (items.Select(i => char.ToUpperInvariant(i.Mnemonic)).Distinct().Count() != items.Count)
                throw new InvalidOperationException($"Mnemonic collision in {path}");
        }
    }
    public static string GenerateMarkdown()
    {
        var text = new StringBuilder("# Demo keyboard map\n\nGenerated from the JSON profiles; the headless test prevents drift.\n");
        foreach (var name in new[] { "desktop", "browser" })
        {
            var profile = Load(name);
            text.Append(System.Globalization.CultureInfo.InvariantCulture, $"\n## {profile.Name}\n\nAlt menus: {(profile.AltMenus ? "enabled" : "disabled; use the menu buttons")}\n\n| Chord | Command |\n| --- | --- |\n");
            foreach (var b in profile.Bindings) text.Append(System.Globalization.CultureInfo.InvariantCulture, $"| {b.Chord} | `{b.CommandId}` |\n");
        }
        text.Append("\nDesktop: Alt, T, P opens Transactions → Procurement; R invokes Receive Goods. " +
            "Arrows navigate, Enter opens or invokes, Escape returns one menu level. " +
            "Launcher: arrows select, Enter invokes, Escape restores focus. " +
            "Dialogs trap Tab focus and Escape cancels. AltGr is never captured.\n");
        return text.ToString();
    }
}
