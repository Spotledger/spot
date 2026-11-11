namespace Spotledger.Client.Shell;

/// <summary>
/// One command in the client's single command model: the menu, the launcher, keyboard
/// shortcuts and tab/context menus all dispatch the same <see cref="Id"/> through the
/// <see cref="CommandDispatcher"/>.
/// </summary>
/// <param name="Id">Stable id; the backend's command id where one exists (ADR-055).</param>
/// <param name="Label">Short name shown in menus and launcher rows.</param>
/// <param name="Description">One-line caption under the label.</param>
/// <param name="Category">Launcher group: Create, Open, Reports or Recent.</param>
/// <param name="MenuPath">Menu branch path ('', "File", "Transactions/Purchasing");
/// empty means the command is not in any menu.</param>
/// <param name="Mnemonic">Access key within <paramref name="MenuPath"/>; ' ' when the
/// command has no menu position.</param>
/// <param name="RequiredContext">Context tag the dispatcher checks before running
/// (e.g. "tab"); null when no context is required.</param>
public sealed record CommandDefinition(
    string Id,
    string Label,
    string Description,
    string Category,
    string MenuPath,
    char Mnemonic,
    string? RequiredContext = null)
{
    /// <summary>Words the launcher matches beyond label and description.</summary>
    public IReadOnlyList<string> SearchTerms { get; init; } = [];
}

/// <summary>A menu branch (a submenu header) at a path, with its own mnemonic.</summary>
public sealed record MenuBranch(string Path, string Label, char Mnemonic);
