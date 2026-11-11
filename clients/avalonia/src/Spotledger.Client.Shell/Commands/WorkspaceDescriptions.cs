namespace Spotledger.Client.Shell;

/// <summary>One honest line per workspace: what it will hold until its screens arrive.</summary>
public static class WorkspaceDescriptions
{
    private static readonly Dictionary<string, string> Descriptions = new(StringComparer.Ordinal)
    {
        ["Home"] = "The role-aware task tiles and KPI cards arrive with WP-UI-12.",
        ["My Tasks"] = "Assigned work with due dates arrives with WP-BPM-08.",
        ["Selling"] = "Quotes, sales orders and invoices arrive with the sales screens (WP-UI-03+).",
        ["Buying"] = "Purchase requests, orders and receipts arrive with the purchasing screens (WP-UI-03+).",
        ["Stock"] = "Stock ledgers and movements arrive with the stock screens (WP-UI-03+).",
        ["Accounts"] = "The general ledger, journals and closing arrive with the accounting screens (WP-UI-03+).",
        ["Reports"] = "Reports arrive with WP-UI-10; each states its source and as-of time.",
        ["Settings"] = "Company, establishment, numbering and user settings arrive with WP-ADM-01.",
    };

    public static string For(string workspace) =>
        Descriptions.TryGetValue(workspace, out var text) ? text : "This workspace arrives with a later task.";
}
