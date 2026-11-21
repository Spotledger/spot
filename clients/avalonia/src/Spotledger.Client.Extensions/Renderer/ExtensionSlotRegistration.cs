using Avalonia.Controls;
using Spotledger.Client.Extensions;

namespace Spotledger.Client.Extensions.Renderer;

/// <summary>
/// The one entry point the shell calls: it registers a host for every slot the document
/// populates, or — when the document was refused whole — shows the "update the app" notice
/// and registers nothing. The shell project is not owned by this task, so the seam is
/// <see cref="IShellSlotRegistry" /> (defined beside the model) and what the shell must
/// provide is listed in the task's open questions.
/// </summary>
public static class ExtensionSlots
{
    /// <summary>
    /// Registers the extension hosts of <paramref name="set" /> with the shell.
    /// </summary>
    /// <param name="registry">The shell's slot registry.</param>
    /// <param name="set">The resolved document.</param>
    /// <param name="context">The values the expressions read (the context is shared by the hosts).</param>
    /// <param name="services">The dispatcher, shell services and query runner.</param>
    /// <returns>The slots that got a host, in the document's slot order.</returns>
    public static IReadOnlyList<string> Register(
        IShellSlotRegistry registry,
        ExtensionSet set,
        ExtensionContext context,
        ExtensionServices services)
    {
        ArgumentNullException.ThrowIfNull(registry);
        ArgumentNullException.ThrowIfNull(set);
        ArgumentNullException.ThrowIfNull(context);
        ArgumentNullException.ThrowIfNull(services);
        if (!set.IsEnabled)
        {
            ExtensionDiagnostic? refusal = set.Diagnostics.Count > 0 ? set.Diagnostics[0] : null;
            registry.ShowNotice(refusal?.Code ?? ExtensionCodes.UnsupportedContract,
                refusal?.Message ?? "the extension document was refused");
            return [];
        }
        List<string> registered = [];
        foreach (string slot in set.PopulatedSlots)
        {
            SlotSpec? spec = ExtensionSlotCatalog.Find(slot);
            if (spec is null || spec.Accepts == ExtensionItemKind.Tab)
            {
                // Tabs need a tab strip in the shell (see the task's open questions); the model
                // carries them so the shell task can wire them without a contract change.
                continue;
            }
            registry.RegisterSlotHost(slot, doctype => Build(spec, set, context, services, doctype));
            registered.Add(slot);
        }
        return registered;
    }

    /// <summary>
    /// Builds one slot's host for one DocType. Returns null when the slot is DocType-scoped
    /// and the shell asked without a DocType, or when nothing is visible for that DocType —
    /// the shell then draws no extension chrome at all.
    /// </summary>
    public static Control? BuildHost(
        ExtensionSet set,
        SlotSpec spec,
        ExtensionContext context,
        ExtensionServices services,
        string? doctype)
    {
        ArgumentNullException.ThrowIfNull(set);
        ArgumentNullException.ThrowIfNull(spec);
        ArgumentNullException.ThrowIfNull(context);
        ArgumentNullException.ThrowIfNull(services);
        if (spec.RequiresDoctype && string.IsNullOrEmpty(doctype))
        {
            return null;
        }
        SlotView view = set.ForSlot(spec.Name, context, doctype, services);
        if (view.Entries.Count == 0)
        {
            view.Dispose();
            return null;
        }
        switch (spec.Accepts)
        {
            case ExtensionItemKind.Tile:
                {
                    TileHost host = new();
                    host.Show(view);
                    return host;
                }
            case ExtensionItemKind.Panel:
                {
                    SidePanelHost host = new();
                    host.Show(view);
                    return host;
                }
            default:
                {
                    ActionBar bar = new();
                    bar.Show(view);
                    return bar;
                }
        }
    }

    private static Control? Build(
        SlotSpec spec,
        ExtensionSet set,
        ExtensionContext context,
        ExtensionServices services,
        string? doctype) => BuildHost(set, spec, context, services, doctype);
}
