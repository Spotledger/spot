using System.Globalization;

namespace Spotledger.Client.Shell;

/// <summary>One row in the launcher: a command with its caption, availability reason and group.</summary>
public sealed record LauncherEntry(CommandDefinition Definition, string Group, CommandAvailability Availability)
{
    public string Id => Definition.Id;

    public string Label => Definition.Label;

    public string Caption => Availability.Reason ?? Definition.Description;

    public string CategoryChip => Definition.Category;
}

/// <summary>
/// The Ctrl+K floating palette: search field, grouped results (Create, Open, Reports,
/// Recent), keyboard selection, Enter to run, Escape to close. Runs entirely through the
/// dispatcher — a palette row is a command trigger like a menu item, with the same reason
/// when it cannot run.
/// </summary>
public sealed class LauncherViewModel : ObservableObject
{
    private string _query = string.Empty;
    private int _selectedIndex = -1;

    public LauncherViewModel(CommandRegistry registry, CommandDispatcher dispatcher)
    {
        Registry = registry ?? throw new ArgumentNullException(nameof(registry));
        Dispatcher = dispatcher ?? throw new ArgumentNullException(nameof(dispatcher));
    }

    public CommandRegistry Registry { get; }

    public CommandDispatcher Dispatcher { get; }

    /// <summary>The shell state the availability check runs against; set when opened.</summary>
    public Func<CommandContext> ContextProvider { get; set; } = () => new CommandContext();

    /// <summary>Recent items shown when the query is empty (open document tabs, newest first).</summary>
    public IReadOnlyList<WorkspaceTabViewModel> Recents { get; set; } = [];

    public IReadOnlyList<LauncherEntry> Items { get; private set; } = [];

    /// <summary>Items grouped for display in the mockup's order: Create, Open, Reports, Recent.</summary>
    public IReadOnlyList<(string Title, IReadOnlyList<LauncherEntry> Entries)> Groups { get; private set; } = [];

    /// <summary>Raised when the result list or the highlighted row changed (the palette rebuilds).</summary>
    public event EventHandler? StateChanged;

    private void RaiseStateChanged() => StateChanged?.Invoke(this, EventArgs.Empty);

    public string Query
    {
        get => _query;
        set
        {
            if (SetField(ref _query, value))
            {
                Refresh();
            }
        }
    }

    public int SelectedIndex
    {
        get => _selectedIndex;
        private set
        {
            if (SetField(ref _selectedIndex, value))
            {
                OnPropertyChanged(nameof(Selected));
                RaiseStateChanged();
            }
        }
    }

    public LauncherEntry? Selected
    {
        get
        {
            var index = SelectedIndex;
            return index >= 0 && index < Items.Count ? Items[index] : null;
        }
    }

    public bool HasItems => Items.Count > 0;

    /// <summary>True while the palette shows the keymap table (Help/Tools → Keyboard map).</summary>
    public bool ShowingKeymap => _keymapProfile is not null;

    private KeymapProfile? _keymapProfile;

    /// <summary>Switch the palette to the keymap viewer of the active profile.</summary>
    public void ShowKeymap(KeymapProfile profile)
    {
        _keymapProfile = profile;
        OnPropertyChanged(nameof(ShowingKeymap));
        OnPropertyChanged(nameof(KeymapName));
    }

    public void HideKeymap()
    {
        _keymapProfile = null;
        OnPropertyChanged(nameof(ShowingKeymap));
    }

    public string KeymapName => _keymapProfile?.Name ?? string.Empty;

    public IReadOnlyList<KeyBinding> KeymapBindings => _keymapProfile?.Bindings ?? [];

    /// <summary>Rebuilds the grouped result list for the current query.</summary>
    public void Refresh()
    {
        HideKeymap();
        List<LauncherEntry> entries = [];
        var query = Query.Trim();
        if (query.Length == 0)
        {
            foreach (var definition in Registry.Definitions
                         .Where(definition => definition.Category is "Create" or "Open" or "Reports"))
            {
                entries.Add(Entry(definition, definition.Category));
            }

            foreach (var tab in Recents)
            {
                entries.Add(new LauncherEntry(
                    new CommandDefinition("tab.focus." + tab.Key, tab.Title, "Focus this open tab", "Recent", "", ' '),
                    "Recent",
                    CommandAvailability.Available));
            }
        }
        else
        {
            foreach (var definition in Registry.Definitions.Where(definition => Matches(definition, query)))
            {
                entries.Add(Entry(definition, definition.Category));
            }
        }

        Items = entries;
        Groups = entries
            .GroupBy(entry => entry.Group)
            .OrderBy(group => GroupOrder(group.Key))
            .Select(group => (group.Key, (IReadOnlyList<LauncherEntry>)group.ToList()))
            .ToList();
        // Keep the flat order aligned with the grouped display order, and highlight the
        // first *available* row: Enter must act on what the badge marks, and a greyed
        // blocked row is never the default target.
        Items = Groups.SelectMany(group => group.Item2).ToList();
        var firstAvailable = -1;
        for (var index = 0; index < Items.Count; index++)
        {
            if (Items[index].Availability.IsAvailable)
            {
                firstAvailable = index;
                break;
            }
        }

        SelectedIndex = Items.Count > 0 ? Math.Max(0, firstAvailable) : -1;
        OnPropertyChanged(nameof(HasItems));
        OnPropertyChanged(nameof(ShowingKeymap));
        RaiseStateChanged();
    }

    private static int GroupOrder(string group) => group switch
    {
        "Create" => 0,
        "Open" => 1,
        "Reports" => 2,
        "Recent" => 3,
        _ => 4,
    };

    private LauncherEntry Entry(CommandDefinition definition, string group) =>
        new(definition, group, Dispatcher.GetAvailability(definition.Id, ContextProvider()));

    /// <summary>Move the highlight; wraps like the mockup's arrow selection.</summary>
    public void MoveSelection(int delta)
    {
        if (Items.Count == 0)
        {
            return;
        }

        var next = (SelectedIndex + delta + Items.Count) % Items.Count;
        SelectedIndex = next;
    }

    /// <summary>Set the highlight directly (pointer selection in the palette).</summary>
    public void SelectIndex(int index) => SelectedIndex = index;

    /// <summary>Runs the highlighted command; returns the dispatch outcome (null = nothing selected).</summary>
    public async Task<DispatchResult?> RunSelectedAsync(CommandRoute route)
    {
        LauncherEntry? entry = Selected;
        if (entry is null)
        {
            return null;
        }

        if (entry.Id.StartsWith("tab.focus.", StringComparison.Ordinal))
        {
            var key = entry.Id["tab.focus.".Length..];
            return await Dispatcher.ExecuteAsync("tab.focus", ContextProvider() with { FocusTabKey = key }, route);
        }

        return await Dispatcher.ExecuteAsync(entry.Id, ContextProvider(), route);
    }

    private static bool Matches(CommandDefinition definition, string query)
    {
        var haystack = string.Join(' ', [definition.Label, definition.Description, .. definition.SearchTerms]);
        return CultureInfo.InvariantCulture.CompareInfo.IndexOf(haystack, query, CompareOptions.IgnoreCase) >= 0;
    }
}
