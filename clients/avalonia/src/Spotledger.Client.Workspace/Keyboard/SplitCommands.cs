namespace Spotledger.Client.Workspace.Keyboard;

/// <summary>
/// The commands the keyboard-first workspace adds to the shell's one registry (WP-UI-17.T2). They are
/// registered through <see cref="Shell.CommandRegistry"/> like any chrome command, so the menu, the
/// launcher and the shortcut column read them from the same place; the chords that reach them are
/// registered through <see cref="WorkspaceKeymaps"/>, never by editing the shipped profile JSON.
/// </summary>
public static class SplitCommands
{
    /// <summary>List only: the form pane folds away.</summary>
    public const string ModeList = "split.mode.list";

    /// <summary>List beside the form.</summary>
    public const string ModeSplit = "split.mode.split";

    /// <summary>Form only: the list folds away.</summary>
    public const string ModeForm = "split.mode.form";

    /// <summary>Select the next row of the list, from wherever the focus is.</summary>
    public const string WalkNext = "split.walk.next";

    /// <summary>Select the previous row of the list, from wherever the focus is.</summary>
    public const string WalkPrevious = "split.walk.previous";

    /// <summary>Focus the list pane.</summary>
    public const string FocusList = "split.focus.list";

    /// <summary>Focus the form pane's first field.</summary>
    public const string FocusForm = "split.focus.form";

    /// <summary>Move the selected document into its own tab and keep the list.</summary>
    public const string OpenInTab = "split.open.inTab";

    /// <summary>Create a new document of the list's DocType, in the form pane.</summary>
    public const string NewFromList = "split.new";

    /// <summary>Save the form and walk to the next row (a worklist workflow).</summary>
    public const string SaveAndNext = "split.saveAndNext";

    /// <summary>F6: the next pane of the window.</summary>
    public const string PaneNext = "pane.next";

    /// <summary>Shift+F6: the previous pane of the window.</summary>
    public const string PanePrevious = "pane.previous";

    /// <summary>Every id this project registers, in menu order.</summary>
    public static readonly string[] Ids =
    [
        ModeList, ModeSplit, ModeForm,
        WalkNext, WalkPrevious,
        FocusList, FocusForm,
        OpenInTab, NewFromList, SaveAndNext,
        PaneNext, PanePrevious,
    ];

    /// <summary>The definitions this project adds to the registry.</summary>
    public static IReadOnlyList<Shell.CommandDefinition> Definitions { get; } = Build();

    /// <summary>Adds every definition to <paramref name="registry"/>; a duplicate id fails there.</summary>
    public static void Register(Shell.CommandRegistry registry)
    {
        ArgumentNullException.ThrowIfNull(registry);
        foreach (var definition in Definitions)
        {
            registry.Add(definition);
        }
    }

    private static Shell.CommandDefinition[] Build() =>
    [
        // The mnemonics are blank on purpose: these commands are reached by their chords and from the
        // launcher, and a second letter at the View level would collide with the chrome's own menu.
        Def(ModeList, "List only", "Fold the form pane away and work the list", "View", "View/Split", "list only", "collapse form"),
        Def(ModeSplit, "List and form", "The list beside the form of the selected row", "View", "View/Split", "split", "side by side"),
        Def(ModeForm, "Form only", "Fold the list away and work the form", "View", "View/Split", "form only", "full form"),
        Def(WalkNext, "Next document", "Select the next row of the list", "Edit", "View", "next row", "walk"),
        Def(WalkPrevious, "Previous document", "Select the previous row of the list", "Edit", "View", "previous row", "walk"),
        Def(FocusList, "Focus the list", "Move focus into the list", "View", "View", "focus list"),
        Def(FocusForm, "Focus the form", "Move focus to the first field of the form", "View", "View", "focus form"),
        Def(OpenInTab, "Open in a tab", "Move the form to its own tab and keep the list", "Open", "View", "tab", "new tab"),
        Def(NewFromList, "New from the list", "Create a document of this list's DocType", "Create", "View", "new document"),
        Def(SaveAndNext, "Save and next", "Save the form and select the next row", "Edit", "View", "save and next", "worklist"),
        Def(PaneNext, "Next pane", "Move focus to the next pane of the window", "View", "View", "pane", "f6"),
        Def(PanePrevious, "Previous pane", "Move focus to the previous pane of the window", "View", "View", "pane", "shift f6"),
    ];

    private static Shell.CommandDefinition Def(string id, string label, string description, string category, string menuPath, params string[] search)
        => new(id, label, description, category, menuPath, ' ') { SearchTerms = search };
}
