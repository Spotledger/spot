using Spotledger.Client.Shell;

namespace Spotledger.Client.Assistant;

/// <summary>The assistant's entry in the shell's single command model.</summary>
public static class AssistantCommands
{
    /// <summary>The command id for the dock toggle.</summary>
    public const string Toggle = "assistant.toggle";

    /// <summary>The default chord that toggles the dock.</summary>
    public const string DefaultChord = "Ctrl+Shift+A";

    /// <summary>The command definition for the palette and menus.</summary>
    public static CommandDefinition ToggleDefinition { get; } = new(
        Toggle,
        "Ask the assistant",
        "Open or close the support assistant",
        "Open",
        string.Empty,
        ' ')
    {
        SearchTerms = ["help", "support", "assistant", "fix", "diagnose"],
    };

    /// <summary>Registers the command (call once at startup).</summary>
    public static void Register(CommandRegistry registry)
    {
        ArgumentNullException.ThrowIfNull(registry);
        registry.Add(ToggleDefinition);
    }
}
