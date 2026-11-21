namespace Spotledger.Client.Extensions;

/// <summary>What a slot accepts. The contract's item kinds, as a closed set.</summary>
public enum ExtensionItemKind
{
    /// <summary>An action: exactly one effect.</summary>
    Action,
    /// <summary>A panel: an ordered list of blocks.</summary>
    Panel,
    /// <summary>A home tile: a composite component with props.</summary>
    Tile,
    /// <summary>A document tab: a label and a panel.</summary>
    Tab
}

/// <summary>One slot of the closed list, with the limits the client enforces again.</summary>
/// <param name="Name">The slot's full name, e.g. <c>doc.toolbar</c>.</param>
/// <param name="Accepts">The only item kind the slot takes.</param>
/// <param name="MaxPerApp">How many items one app may put in the slot (per DocType in <c>doc.*</c>/<c>list.*</c>).</param>
/// <param name="LabelMax">The longest literal label, in characters.</param>
public sealed record SlotSpec(string Name, ExtensionItemKind Accepts, int MaxPerApp, int LabelMax)
{
    /// <summary>True when the slot is scoped to a DocType, so an item must name one.</summary>
    public bool RequiresDoctype => Name.StartsWith("doc.", StringComparison.Ordinal) || Name.StartsWith("list.", StringComparison.Ordinal);
}

/// <summary>
/// The closed slot list of contract 0.1.0 and the reference implementation's limits,
/// mirrored from <c>contracts/ui-ext/schema/slot.json</c> and <c>errors.md</c>. A test
/// reads the schema files and fails when the two drift, so this table is never a guess.
/// </summary>
public static class ExtensionSlotCatalog
{
    /// <summary>The shell's sidebar.</summary>
    public const string ShellSidebar = "shell.sidebar";
    /// <summary>The shell's menu bar.</summary>
    public const string ShellMenubar = "shell.menubar";
    /// <summary>The shell's status bar.</summary>
    public const string ShellStatusbar = "shell.statusbar";
    /// <summary>The shell's user menu.</summary>
    public const string ShellUserMenu = "shell.user-menu";
    /// <summary>The home screen's tiles.</summary>
    public const string HomeTiles = "home.tiles";
    /// <summary>A document's toolbar.</summary>
    public const string DocToolbar = "doc.toolbar";
    /// <summary>A document's side panel.</summary>
    public const string DocSidepanel = "doc.sidepanel";
    /// <summary>A document's tabs.</summary>
    public const string DocTab = "doc.tab";
    /// <summary>A list's toolbar.</summary>
    public const string ListToolbar = "list.toolbar";
    /// <summary>A list's side panel.</summary>
    public const string ListSidepanel = "list.sidepanel";

    /// <summary>Every slot, in the contract's order.</summary>
    public static IReadOnlyList<SlotSpec> All { get; } =
    [
        new(ShellSidebar, ExtensionItemKind.Action, 10, 32),
        new(ShellMenubar, ExtensionItemKind.Action, 10, 32),
        new(ShellStatusbar, ExtensionItemKind.Action, 3, 24),
        new(ShellUserMenu, ExtensionItemKind.Action, 5, 32),
        new(HomeTiles, ExtensionItemKind.Tile, 8, 40),
        new(DocToolbar, ExtensionItemKind.Action, 6, 24),
        new(DocSidepanel, ExtensionItemKind.Panel, 3, 32),
        new(DocTab, ExtensionItemKind.Tab, 3, 24),
        new(ListToolbar, ExtensionItemKind.Action, 4, 24),
        new(ListSidepanel, ExtensionItemKind.Panel, 3, 32)
    ];

    /// <summary>Finds a slot by its full name; null when the name is not in the closed list.</summary>
    public static SlotSpec? Find(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        foreach (SlotSpec slot in All)
        {
            if (string.Equals(slot.Name, name, StringComparison.Ordinal))
            {
                return slot;
            }
        }
        return null;
    }
}

/// <summary>The limits of contract 0.1.0, mirrored from the schemas and the reference validator.</summary>
public static class ExtensionLimits
{
    /// <summary>The contract version this client implements.</summary>
    public const string ContractVersion = "0.1.0";
    /// <summary>The major of <see cref="ContractVersion"/>; a document with a newer major is disabled whole.</summary>
    public const int ContractMajor = 0;
    /// <summary>The shell contract this client reports: the same version, since T1 and T3 ship together.</summary>
    public const string ShellContract = "0.1.0";
    /// <summary>The largest document the client parses, in bytes (<c>spot-ui-ext</c>'s limit).</summary>
    public const int MaxDocumentBytes = 1024 * 1024;
    /// <summary>One expansion nests at most this deep.</summary>
    public const int MaxComponentDepth = 8;
    /// <summary>One expansion has at most this many nodes; a <c>repeat</c> counts <c>max</c> times its body.</summary>
    public const int MaxComponentNodes = 500;
    /// <summary>A <c>repeat</c> repeats its body at most this many times.</summary>
    public const int MaxRepeat = 50;
    /// <summary>A panel has at most this many blocks.</summary>
    public const int MaxPanelBlocks = 12;
    /// <summary>A table block has at most this many columns.</summary>
    public const int MaxTableColumns = 8;
    /// <summary>A table block holds at most this many rows.</summary>
    public const int MaxTableRows = 50;
    /// <summary>A field list or key/value block has at most this many entries.</summary>
    public const int MaxListEntries = 20;
    /// <summary>A call or filter map has at most this many entries.</summary>
    public const int MaxArguments = 8;
    /// <summary>A composite's <c>use</c> node has at most this many filled slots.</summary>
    public const int MaxFillSlots = 8;
    /// <summary>A component declares at most this many props.</summary>
    public const int MaxProps = 16;
    /// <summary>A layout node has at most this many children.</summary>
    public const int MaxChildren = 100;
    /// <summary>A grid has at most this many columns.</summary>
    public const int MaxGridColumns = 6;
    /// <summary>A label is at most this many characters before the slot's own limit applies.</summary>
    public const int MaxLabel = 64;
    /// <summary>An expression string is at most this many characters.</summary>
    public const int MaxExpression = 500;
    /// <summary>A text block's markdown is at most this many characters.</summary>
    public const int MaxMarkdown = 2000;
    /// <summary>A confirm text is at most this many characters.</summary>
    public const int MaxConfirm = 200;

    /// <summary>The closed icon set: neutral tokens, never URLs or paths.</summary>
    public static IReadOnlySet<string> IconTokens { get; } = new HashSet<string>(StringComparer.Ordinal)
    {
        "add", "edit", "delete", "save", "search", "filter", "refresh", "download", "upload", "print",
        "share", "settings", "user", "users", "home", "folder", "file", "list", "grid", "calendar",
        "clock", "check", "close", "warning", "info", "star", "flag", "tag", "link", "mail",
        "phone", "chart", "money", "box", "truck", "cart", "wrench", "play", "pause", "archive",
        "lock", "unlock", "eye", "attach", "comment", "history", "more", "arrow_left", "arrow_right", "tasks",
        "project", "time"
    };

    /// <summary>The controls a composite may use: the read-only catalog presentations plus a button.</summary>
    public static IReadOnlySet<string> ComponentControls { get; } = new HashSet<string>(StringComparer.Ordinal)
    {
        "label", "heading", "separator", "badge", "progress", "number", "currency", "date", "button"
    };

    /// <summary>The layout node kinds of a composite body.</summary>
    public static IReadOnlySet<string> LayoutNodes { get; } = new HashSet<string>(StringComparer.Ordinal)
    {
        "row", "column", "grid", "group"
    };
}
