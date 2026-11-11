namespace Spotledger.Client.Shell;

/// <summary>
/// The command catalog the shell chrome registers. Every id here is a chrome-level command
/// that genuinely exists in this build, or a backend command id (ADR-055) whose screen the
/// product has not shipped yet. The brief's rule: a product screen that is not in the
/// product is **absent** — so there is no Manufacturing/Projects/Accounting submenu, no
/// POS entry and no process action. Where a real command id exists but the form runtime is
/// not built, the item shows greyed with its reason (never an inert click), same for
/// chrome commands whose context is missing right now (no tab focused, no text field).
/// </summary>
public static class ShellCommands
{
    /// <summary>Commands that act on the active tab; the availability provider blocks them
    /// with a reason when no tab is selected.</summary>
    public static readonly string[] TabScopedCommands =
    [
        "tab.close", "tab.others", "tab.all", "tab.pin", "tab.duplicate", "tab.newWindow",
        "tab.next", "tab.previous",
    ];

    /// <summary>Commands that act on a focused text field (Edit menu).</summary>
    public static readonly string[] TextScopedCommands =
    [
        "edit.cut", "edit.copy", "edit.paste", "edit.selectAll",
    ];

    /// <summary>Backend document commands whose forms arrive with later tasks: registered
    /// with their real ids, shown greyed with the reason the shell cannot open them yet.</summary>
    public static readonly string[] DocumentCreateCommands =
    [
        "create.purchaseRequest", "create.supplierQuotation", "create.purchaseOrder",
        "create.goodsReceipt", "create.supplierInvoice", "create.supplierPayment",
        "create.salesInvoice", "create.customer",
        "create.stockEntry", "create.stockReconciliation",
    ];

    public static CommandRegistry BuildRegistry()
    {
        var registry = new CommandRegistry();
        foreach (var branch in Branches)
        {
            registry.AddBranch(branch);
        }

        foreach (var definition in Definitions)
        {
            registry.Add(definition);
        }

        return registry;
    }

    /// <summary>The eight workspaces of the sidebar (brief deliverable 5).</summary>
    public static readonly (string CommandId, string Label)[] Workspaces =
    [
        ("workspace.home", "Home"),
        ("workspace.myTasks", "My Tasks"),
        ("workspace.selling", "Selling"),
        ("workspace.buying", "Buying"),
        ("workspace.stock", "Stock"),
        ("workspace.accounts", "Accounts"),
        ("workspace.reports", "Reports"),
        ("workspace.settings", "Settings"),
    ];

    private static MenuBranch Branch(string path, char mnemonic)
    {
        var label = path.Contains('/', StringComparison.Ordinal) ? path[(path.LastIndexOf('/') + 1)..] : path;
        return new MenuBranch(path, label, mnemonic);
    }

    private static readonly MenuBranch[] Branches =
    [
        Branch("File", 'F'),
        Branch("Edit", 'E'),
        Branch("View", 'V'),
        Branch("Transactions", 'T'),
        Branch("Transactions/Purchasing", 'P'),
        Branch("Transactions/Sales", 'S'),
        Branch("Transactions/Stock", 'K'),
        Branch("Processes", 'O'),
        Branch("Reports", 'R'),
        Branch("Tools", 'U'),
        Branch("Window", 'W'),
        Branch("Help", 'H'),
    ];

    private static CommandDefinition Def(
        string id, string label, string category, string menuPath, char mnemonic, string description,
        string requiredContext = "", string[]? search = null) =>
        new(id, label, description, category, menuPath, mnemonic,
            string.IsNullOrEmpty(requiredContext) ? null : requiredContext)
        {
            SearchTerms = search ?? [],
        };

    private static readonly CommandDefinition[] Definitions =
    [
        // File: shell-level actions only. Exit is owned by the window manager, so it is absent.
        Def("shell.launcher", "Command launcher", "Open", "File", 'C', "Search commands and open workspaces", search: ["find", "search", "palette"]),
        Def("shell.logout", "Sign out", "Open", "File", 'S', "End the session and return to sign in", search: ["sign out"]),

        // Edit: the clipboard commands act on the focused text field (search box, launcher).
        Def("edit.cut", "Cut", "Open", "Edit", 'T', "Cut the selection to the clipboard", "text"),
        Def("edit.copy", "Copy", "Open", "Edit", 'C', "Copy the selection to the clipboard", "text"),
        Def("edit.paste", "Paste", "Open", "Edit", 'P', "Paste from the clipboard", "text"),
        Def("edit.selectAll", "Select all", "Open", "Edit", 'A', "Select the whole field", "text"),

        // View: sidebar cycling and the workspaces (all real).
        Def("view.sidebar", "Toggle sidebar", "Open", "View", 'S', "Cycle expanded, icons-only and hidden", search: ["rail", "collapse"]),
        Def("workspace.home", "Home", "Open", "View", 'H', "Open the Home workspace", search: ["start", "dashboard"]),
        Def("workspace.myTasks", "My Tasks", "Open", "View", 'M', "Open the My Tasks workspace", search: ["tasks", "todo"]),
        Def("workspace.selling", "Selling", "Open", "View", 'E', "Open the Selling workspace"),
        Def("workspace.buying", "Buying", "Open", "View", 'B', "Open the Buying workspace", search: ["purchase", "buy"]),
        Def("workspace.stock", "Stock", "Open", "View", 'K', "Open the Stock workspace", search: ["inventory"]),
        Def("workspace.accounts", "Accounts", "Open", "View", 'A', "Open the Accounts workspace", search: ["finance"]),
        Def("workspace.reports", "Reports", "Open", "View", 'R', "Open the Reports workspace"),
        Def("workspace.settings", "Settings", "Open", "View", 'G', "Open the Settings workspace", search: ["preferences"]),

        // Transactions → the three submenus of the mockup. The document ids are the
        // backend's; their forms arrive with later UI tasks, so they list greyed.
        // Purchasing keeps the mockup's accelerators: R, Q, O, G, I, Y.
        Def("create.purchaseRequest", "Purchase Request", "Create", "Transactions/Purchasing", 'R', "Raise a material request", search: ["buy materials", "request"]),
        Def("create.supplierQuotation", "Supplier Quotation", "Create", "Transactions/Purchasing", 'Q', "Ask a supplier for prices", search: ["quote"]),
        Def("create.purchaseOrder", "Purchase Order", "Create", "Transactions/Purchasing", 'O', "Order goods from a supplier", search: ["order stock"]),
        Def("create.goodsReceipt", "Goods Receipt", "Create", "Transactions/Purchasing", 'G', "Record incoming goods", search: ["receive stock"]),
        Def("create.supplierInvoice", "Supplier Invoice", "Create", "Transactions/Purchasing", 'I', "Enter a supplier bill", search: ["supplier bill"]),
        Def("create.supplierPayment", "Supplier Payment", "Create", "Transactions/Purchasing", 'Y', "Prepare a supplier payment", search: ["pay supplier"]),
        Def("create.salesInvoice", "Sales Invoice", "Create", "Transactions/Sales", 'I', "Invoice a customer", search: ["new sale", "sell"]),
        Def("create.customer", "Customer", "Create", "Transactions/Sales", 'C', "Register a customer", search: ["new party"]),
        Def("create.stockEntry", "Stock Entry", "Create", "Transactions/Stock", 'E', "Move stock", search: ["material transfer"]),
        Def("create.stockReconciliation", "Stock Reconciliation", "Create", "Transactions/Stock", 'R', "Correct stock counts", search: ["recount"]),

        // Processes: the My Tasks workspace is real; the case runtime (WP-BPM-08) is absent.
        Def("process.myTasks", "My Tasks", "Open", "Processes", 'M', "Assigned work and due dates", search: ["tasks", "todo"]),
        Def("process.cases", "Process cases", "Open", "Processes", 'C', "Procure-to-Pay and Order-to-Cash cases"),

        // Reports: the workspace is real; a concrete report needs the runtime (WP-UI-10).
        Def("report.purchases", "Purchases", "Reports", "Reports", 'P', "Purchase totals by period", search: ["purchases report"]),

        // Tools: chrome utilities that genuinely exist.
        Def("tools.keymap", "Keyboard map", "Open", "Tools", 'K', "Show the chords of the active keymap profile", search: ["shortcuts"]),
        Def("tools.density", "Toggle density", "Open", "Tools", 'D', "Switch between Compact and Comfortable", search: ["comfortable", "spacing"]),
        Def("tools.locale", "Toggle Urdu (RTL)", "Open", "Tools", 'R', "Switch the shell between English LTR and Urdu RTL", search: ["right to left", "language"]),

        // Window: tab management.
        Def("tab.new", "New tab", "Open", "Window", 'N', "Open the Home workspace in a tab"),
        Def("tab.next", "Next tab", "Open", "Window", 'X', "Move to the next tab", "tab", search: ["cycle"]),
        Def("tab.previous", "Previous tab", "Open", "Window", 'V', "Move to the previous tab", "tab"),
        Def("tab.close", "Close tab", "Open", "Window", 'C', "Close the active tab, asking about unsaved work", "tab"),
        Def("tab.others", "Close other tabs", "Open", "Window", 'O', "Close every unpinned tab except the active one", "tab"),
        Def("tab.all", "Close all tabs", "Open", "Window", 'L', "Close every tab, asking about unsaved work", "tab"),
        Def("tab.pin", "Pin or unpin tab", "Open", "Window", 'I', "Keep the tab during bulk close", "tab"),
        Def("tab.duplicate", "Duplicate tab", "Open", "Window", 'U', "Open another tab of the same record", "tab"),
        Def("tab.newWindow", "Move to New Window", "Open", "Window", 'M', "Open this tab in its own window", "tab"),

        // Help.
        Def("help.keymap", "Keyboard map", "Open", "Help", 'K', "Show the chords of the active keymap profile"),
        Def("help.about", "About Spotledger", "Open", "Help", 'A', "Client and session information"),

        // The New button and the launcher's primary entry.
        Def("shell.new", "New", "Create", "", ' ', "Choose what to create in the launcher", search: ["create"]),

        // F10: move keyboard focus to the menu bar. Keyboard-only, no menu position.
        Def("window.menu", "Focus the menu bar", "Open", "", ' ', "Move focus to the menu bar", search: ["f10"]),

        // The launcher's Recent rows focus an open tab by key; hidden from every menu.
        Def("tab.focus", "Jump to tab", "Recent", "", ' ', "Focus an open tab from the launcher"),
    ];
}
