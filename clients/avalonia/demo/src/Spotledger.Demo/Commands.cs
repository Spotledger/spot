using Spotledger.Demo.Mock.Contracts;
using Spotledger.Demo.Mock;

namespace Spotledger.Demo;

public enum CommandRoute { Menu, AltSequence, Launcher, Shortcut, ContextMenu, Tile }
public sealed record CommandDefinition(string Id, string Label, string Description, string Category,
    IReadOnlyList<string> SearchTerms, string MenuPath, char Mnemonic, string? RequiredContext = null);
public sealed record MenuBranch(string Path, string Label, char Mnemonic);
public sealed class CommandRegistry
{
    private readonly Dictionary<string, CommandDefinition> definitions = new(StringComparer.Ordinal);
    public IReadOnlyCollection<CommandDefinition> Definitions => definitions.Values;
    public static IReadOnlyList<MenuBranch> Branches { get; } =
    [new("File", "File", 'F'), new("Edit", "Edit", 'E'), new("View", "View", 'V'),
     new("Transactions", "Transactions", 'T'), new("Processes", "Processes", 'P'), new("Reports", "Reports", 'R'),
     new("Tools", "Tools", 'O'), new("Window", "Window", 'W'), new("Help", "Help", 'H'), new("Demo", "Demo", 'D'),
     new("Transactions/Procurement", "Procurement", 'P'), new("Transactions/Sales", "Sales", 'S'),
     new("Demo/Role", "Role", 'R'), new("Demo/Keymap", "Keymap", 'K'), new("Demo/Locale", "Locale", 'L'),
     new("Demo/Density", "Density", 'D'), new("Demo/Enforcement", "Enforcement", 'E')];
    public CommandDefinition this[string id] => definitions[id];
    public void Add(CommandDefinition definition) => definitions.Add(definition.Id, definition);
    public static CommandRegistry Create(HomeData home)
    {
        var registry = new CommandRegistry();
        void Add(string id, string label, string category, string path, char mnemonic, string description, params string[] aliases)
            => registry.Add(new(id, label, description, category, aliases, path, mnemonic,
                id.StartsWith("document.", StringComparison.Ordinal) ? "record" : null));
        Add("shell.launcher", "Command launcher", "Open", "File", 'O', "Search commands and recent records", "find", "search");
        Add("document.save", "Save demo note", "Open", "File", 'S', "Save the placeholder note using a document version");
        Add("tab.close", "Close tab", "Open", "File", 'C', "Save, discard or cancel before closing a dirty tab");
        Add("shell.exit", "Exit demo", "Open", "File", 'X', "Ask about dirty tabs before exiting");
        Add("edit.note", "Edit placeholder note", "Open", "Edit", 'E', "Open a seeded document and edit its note");
        Add("view.sidebar", "Toggle sidebar", "Open", "View", 'S', "Cycle expanded, icons and hidden sidebar");
        Add("view.rail", "Toggle right rail", "Open", "View", 'R', "Show or hide the independent context rail");
        Add("workspace.home", "Home", "Open", "View", 'H', "Role-aware tasks and KPIs");
        Add("create.request", "Purchase Request", "Create", "Transactions/Procurement", 'Q', "Start a material request in part 2", "buy materials");
        Add("create.order", "Purchase Order", "Create", "Transactions/Procurement", 'O', "Order goods in part 2", "order stock");
        Add("create.receipt", "Receive Goods", "Create", "Transactions/Procurement", 'R', "Record incoming goods in part 2", "receive stock");
        Add("create.invoice", "Supplier Invoice", "Create", "Transactions/Procurement", 'I', "Review supplier invoices in part 2");
        Add("create.payment", "Pay Supplier", "Create", "Transactions/Procurement", 'P', "Prepare supplier payments in part 2");
        Add("create.sale", "Invoice Customer", "Create", "Transactions/Sales", 'I', "Customer invoice form in part 3", "new sale");
        Add("document.post", "Post document", "Create", "Transactions/Sales", 'P', "Inspect backend posting availability");
        Add("workspace.tasks", "My Tasks", "Open", "Processes", 'T', "Assigned work and due dates");
        Add("workspace.processes", "Processes", "Open", "Processes", 'P', "Procure-to-Pay cases arrive in part 2");
        Add("workspace.approvals", "Approvals", "Open", "Processes", 'A', "Approval evidence in part 2");
        Add("workspace.reports", "Reports", "Reports", "Reports", 'R', "Reporting workspace in part 5");
        Add("report.purchases", "Purchases", "Reports", "Reports", 'P', "Purchase KPI drill-through in part 5");
        Add("report.stock", "Stock value", "Reports", "Reports", 'S', "Stock KPI drill-through in part 5");
        Add("report.receivables", "Receivables", "Reports", "Reports", 'C', "Receivables drill-through in part 5");
        Add("tools.admin", "Administration boundary", "Open", "Tools", 'A', "Admin Center is outside this demo; show its boundary");
        Add("tab.next", "Next tab", "Open", "Window", 'N', "Switch to the next global tab");
        Add("tab.previous", "Previous tab", "Open", "Window", 'P', "Switch to the previous global tab");
        Add("tab.others", "Close other tabs", "Open", "Window", 'O', "Keep this tab and pinned tabs");
        Add("tab.right", "Close tabs to the right", "Open", "Window", 'R', "Close unpinned tabs after this tab");
        Add("tab.pin", "Pin or unpin tab", "Open", "Window", 'I', "Keep the tab during bulk close");
        Add("tab.overflow", "All tabs", "Open", "Window", 'T', "Keyboard-accessible tab overflow");
        Add("help.keymap", "Keyboard map", "Open", "Help", 'K', "View the selected keymap profile");
        Add("help.guide", "Demo guide", "Open", "Help", 'D', "Show supported shell scenarios");
        Add("help.about", "About Spotledger demo", "Open", "Help", 'A', "This mock demonstration is part 1 of WP-UI-18");
        Add("demo.reset", "Reset demo", "Open", "Demo", 'S', "Restore seeded data and close tabs after a dirty prompt");
        Add("demo.governance", "Toggle governance / simple Home", "Open", "Demo", 'G', "Switch full and simple task-tile Home");
        Add("demo.maintenance", "Toggle maintenance", "Open", "Demo", 'M', "Show the maintenance banner");
        foreach (var role in Enum.GetValues<DemoRole>())
            Add($"demo.role.{role}", MockBackend.RoleNames[(int)role], "Open", "Demo/Role", "PAWCSHM"[(int)role], "Switch simulated persona");
        Add("demo.keymap.desktop", "Desktop", "Open", "Demo/Keymap", 'D', "Desktop keyboard profile with Alt menus");
        Add("demo.keymap.browser", "Browser", "Open", "Demo/Keymap", 'B', "Browser-safe keyboard profile");
        Add("demo.locale.en", "English", "Open", "Demo/Locale", 'E', "English left-to-right shell");
        Add("demo.locale.ur", "Urdu", "Open", "Demo/Locale", 'U', "Urdu right-to-left Home and shell");
        Add("demo.density.compact", "Compact", "Open", "Demo/Density", 'C', "Compact design-system density");
        Add("demo.density.comfortable", "Comfortable", "Open", "Demo/Density", 'O', "Comfortable design-system density");
        foreach (var level in Enum.GetValues<EnforcementLevel>())
            Add($"demo.enforcement.{level}", level.ToString(), "Open", "Demo/Enforcement", "GST"[(int)level], "Accounting enforcement banner");
        foreach (var (key, label, mnemonic) in new[] { ("sell", "Sell", 'S'), ("buy", "Buy", 'B'),
                     ("inventory", "Inventory", 'I'), ("favorites", "Favorites", 'F'), ("pos", "Checkout", 'C') })
            Add($"workspace.{key}", label, "Open", "", mnemonic, $"Open {label} workspace");
        foreach (var record in home.Records)
            Add($"record.{record.Id}", $"{record.Title} · {record.Number}", record.IsOpen ? "Open" : "Recent", "", ' ',
                "Open seeded record by UUID", record.Number);
        return registry;
    }
    public IReadOnlyList<(string Label, char Mnemonic, string? CommandId, string? Path)> MenuItems(string path)
    {
        var prefix = path.Length == 0 ? "" : path + "/";
        return Branches.Where(b => b.Path.StartsWith(prefix, StringComparison.Ordinal) && !b.Path[prefix.Length..].Contains('/'))
            .Select(b => (b.Label, b.Mnemonic, (string?)null, (string?)b.Path))
            .Concat(Definitions.Where(d => d.MenuPath == path && path.Length > 0)
                .Select(d => (d.Label, d.Mnemonic, (string?)d.Id, (string?)null))).ToArray();
    }
}
public sealed record DispatchResult(bool Executed, Availability Availability, ApiError? Error = null, bool Duplicate = false);
public sealed class CommandDispatcher(IBackend backend, CommandRegistry registry)
{
    private readonly Dictionary<string, Func<CommandContext, Task>> handlers = new(StringComparer.Ordinal);
    private readonly HashSet<string> running = new(StringComparer.Ordinal);
    public void Register(string id, Func<CommandContext, Task> handler)
    {
        _ = registry[id];
        handlers.Add(id, handler);
    }
    public async Task<DispatchResult> ExecuteAsync(string id, CommandContext context, CommandRoute route)
    {
        _ = registry[id];
        // Route is diagnostic only: every route uses exactly the same availability and handler.
        _ = route;
        lock (running)
            if (!running.Add(id)) return new(false, Availability.Ready, Duplicate: true);
        try
        {
            var reply = await backend.GetAvailabilityAsync(id, context);
            if (!reply.IsSuccess) return new(false, new(AvailabilityState.Blocked, reply.Error!.Message), reply.Error);
            if (!reply.Value!.CanExecute) return new(false, reply.Value);
            await handlers[id](context);
            return new(true, reply.Value);
        }
        finally { lock (running) running.Remove(id); }
    }
}
