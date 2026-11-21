namespace Spotledger.Client.Shell;

/// <summary>
/// The one command registry the whole chrome reads: menu building, the launcher,
/// shortcut lookup and context menus all consult it. Registration fails fast on
/// a duplicate id, a duplicate mnemonic at one menu level, or a duplicate chord
/// in a keymap profile (enforced by tests as well).
/// </summary>
public sealed class CommandRegistry
{
    private readonly Dictionary<string, CommandDefinition> _definitions = new(StringComparer.Ordinal);
    private readonly List<MenuBranch> _branches = [];

    public IReadOnlyCollection<CommandDefinition> Definitions => _definitions.Values;

    public IReadOnlyList<MenuBranch> Branches => _branches;

    public CommandDefinition this[string id] =>
        _definitions.TryGetValue(id, out var definition)
            ? definition
            : throw new KeyNotFoundException($"Command '{id}' is not registered.");

    public bool TryGet(string id, out CommandDefinition? definition) => _definitions.TryGetValue(id, out definition);

    public void AddBranch(MenuBranch branch)
    {
        if (_branches.Any(existing => string.Equals(existing.Path, branch.Path, StringComparison.Ordinal)))
        {
            throw new InvalidOperationException($"Menu branch '{branch.Path}' is already registered.");
        }

        _branches.Add(branch);
        ValidateMnemonics();
    }

    public void Add(CommandDefinition definition)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(definition.Id);
        if (!_definitions.TryAdd(definition.Id, definition))
        {
            throw new InvalidOperationException($"Command '{definition.Id}' is already registered.");
        }

        ValidateMnemonics();
    }

    /// <summary>Menu children of one branch path: sub-branches first, then commands, in registration order.</summary>
    public IReadOnlyList<(string Label, char Mnemonic, string? CommandId, string? Path)> MenuItems(string path)
    {
        var prefix = path.Length == 0 ? string.Empty : path + "/";
        var branches = _branches
            .Where(branch => branch.Path.StartsWith(prefix, StringComparison.Ordinal)
                             && !branch.Path[prefix.Length..].Contains('/', StringComparison.Ordinal))
            .Select(branch => (branch.Label, branch.Mnemonic, (string?)null, (string?)branch.Path));
        var commands = _definitions.Values
            .Where(definition => string.Equals(definition.MenuPath, path, StringComparison.Ordinal)
                                 && path.Length > 0)
            .Select(definition => (definition.Label, definition.Mnemonic, (string?)definition.Id, (string?)null));
        return [.. branches, .. commands];
    }

    /// <summary>Throws when two entries share a mnemonic at one menu level (case-insensitive).</summary>
    public void ValidateMnemonics()
    {
        foreach (var path in _branches.Select(branch => branch.Path).Prepend(string.Empty))
        {
            var items = MenuItems(path)
                .Where(item => item.Mnemonic != ' ')
                .ToList();
            var seen = new HashSet<char>();
            foreach (var item in items)
            {
                var key = char.ToUpperInvariant(item.Mnemonic);
                if (!seen.Add(key))
                {
                    throw new InvalidOperationException(
                        $"Mnemonic collision at menu level '{path}': '{item.Label}' uses '{item.Mnemonic}' twice.");
                }
            }
        }
    }
}
