using System.Text;
using System.Text.Json;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>
/// One resolved tier A document — the site bundle the server sends (WP-UI-13.T2's
/// canonical JSON) or one app's own <c>ui</c> document — parsed into the model the client
/// renders. Parsing never throws: every item the client cannot render is dropped with an
/// <see cref="ExtensionDiagnostic" />, so a malformed or hostile document costs the broken
/// item and nothing else, and a newer contract or shell disables the whole set with one
/// notice instead of a partial rendering.
/// </summary>
public sealed class ExtensionSet
{
    private readonly Dictionary<string, List<IUiItem>> _slots;

    private ExtensionSet(
        string contractVersion,
        string? requiresShell,
        string? shellContract,
        bool isEnabled,
        bool upgradeRequired,
        DiagnosticSink sink,
        List<ActionSpec> actions,
        List<PanelSpec> panels,
        List<ComponentSpec> components,
        List<TileSpec> tiles,
        List<TabSpec> tabs)
    {
        ContractVersion = contractVersion;
        RequiresShell = requiresShell;
        ShellContract = shellContract;
        IsEnabled = isEnabled;
        UpgradeRequired = upgradeRequired;
        Sink = sink;
        Actions = actions;
        Panels = panels;
        Components = components;
        Tiles = tiles;
        Tabs = tabs;
        _slots = [];
        foreach (IUiItem item in Enumerate())
        {
            if (!_slots.TryGetValue(item.Slot, out List<IUiItem>? items))
            {
                items = [];
                _slots[item.Slot] = items;
            }
            items.Add(item);
        }
        foreach (List<IUiItem> items in _slots.Values)
        {
            items.Sort(CompareItems);
        }
    }

    /// <summary>An empty set: nothing to render, no diagnostics.</summary>
    public static ExtensionSet Empty { get; } = CreateEmpty();

    /// <summary>The contract version the document declares (or the client's, when it did not parse).</summary>
    public string ContractVersion { get; }

    /// <summary>The oldest shell the document needs, when it declares one.</summary>
    public string? RequiresShell { get; }

    /// <summary>The shell contract the server resolved for, when the bundle carries it.</summary>
    public string? ShellContract { get; }

    /// <summary>False when the document was refused whole: a newer contract major, or a shell that is too old.</summary>
    public bool IsEnabled { get; }

    /// <summary>True when the client's shell is older than the document's <c>requires_shell</c>.</summary>
    public bool UpgradeRequired { get; }

    /// <summary>Where the document's and the models' diagnostics go; one shared, de-duplicated report.</summary>
    public DiagnosticSink Sink { get; }

    /// <summary>Everything reported so far, in report order.</summary>
    public IReadOnlyList<ExtensionDiagnostic> Diagnostics => Sink.Items;

    /// <summary>The actions of the document, in document order.</summary>
    public IReadOnlyList<ActionSpec> Actions { get; }

    /// <summary>The panels of the document.</summary>
    public IReadOnlyList<PanelSpec> Panels { get; }

    /// <summary>The composite components of the document.</summary>
    public IReadOnlyList<ComponentSpec> Components { get; }

    /// <summary>The home tiles of the document.</summary>
    public IReadOnlyList<TileSpec> Tiles { get; }

    /// <summary>The document tabs of the document.</summary>
    public IReadOnlyList<TabSpec> Tabs { get; }

    /// <summary>The slots that have at least one renderable item.</summary>
    public IReadOnlyList<string> PopulatedSlots => [.. _slots.Keys.Order(StringComparer.Ordinal)];

    /// <summary>
    /// Parses a document. Reference checks run only when <paramref name="facts" /> knows the
    /// site's declarations; structural checks (closed objects, slots, ids, limits, effects,
    /// expressions, markdown, graph and size) always run.
    /// </summary>
    /// <param name="json">The document text.</param>
    /// <param name="facts">The manifest and permission declarations, when the host has them.</param>
    /// <param name="source">The document's name, for messages; <c>ui.json</c> by default.</param>
    public static ExtensionSet Parse(string json, ExtensionFacts? facts = null, string? source = null)
    {
        ArgumentNullException.ThrowIfNull(json);
        DiagnosticSink sink = new();
        string name = source ?? "ui.json";
        if (Encoding.UTF8.GetByteCount(json) > ExtensionLimits.MaxDocumentBytes)
        {
            sink.Report(name, ExtensionCodes.DocumentTooLarge,
                $"the document is {Encoding.UTF8.GetByteCount(json)} bytes; the limit is {ExtensionLimits.MaxDocumentBytes}");
            return Disabled(sink, ExtensionLimits.ContractVersion, null, null);
        }
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json, new JsonDocumentOptions { MaxDepth = 64 });
        }
        catch (JsonException error)
        {
            sink.Report(name, ExtensionCodes.Schema, error.Message);
            return Disabled(sink, ExtensionLimits.ContractVersion, null, null);
        }
        using (document)
        {
            JsonElement root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                sink.Report(name, ExtensionCodes.Schema, "the document is an object");
                return Disabled(sink, ExtensionLimits.ContractVersion, null, null);
            }
            string? version = JsonRead.String(root, "contract_version");
            if (version is null || ExtensionPatterns.MajorOf(version) is not { } major)
            {
                sink.Report(name, ExtensionCodes.Schema, "`contract_version` is a semver string");
                return Disabled(sink, ExtensionLimits.ContractVersion, null, null);
            }
            if (major != ExtensionLimits.ContractMajor)
            {
                sink.Report(name, ExtensionCodes.UnsupportedContract,
                    $"the document needs contract {version}; this client implements {ExtensionLimits.ContractVersion}");
                return Disabled(sink, version, JsonRead.String(root, "requires_shell"), JsonRead.String(root, "shell_contract"));
            }
            string? requiresShell = JsonRead.String(root, "requires_shell");
            string? shellContract = JsonRead.String(root, "shell_contract") ?? ExtensionLimits.ShellContract;
            if (requiresShell is not null &&
                ExtensionPatterns.Compare(ExtensionLimits.ShellContract, requiresShell) is < 0)
            {
                sink.Report(name, ExtensionCodes.UnsupportedContract,
                    $"the document needs shell {requiresShell}; this shell is {ExtensionLimits.ShellContract}");
                return new ExtensionSet(version, requiresShell, shellContract, false, true, sink, [], [], [], [], []);
            }
            ExtensionParser parser = new(facts ?? ExtensionFacts.None, sink, name);
            parser.Parse(root);
            return new ExtensionSet(version, requiresShell, shellContract, true, false, sink,
                parser.Actions, parser.Panels, parser.Components, parser.Tiles, parser.Tabs);
        }
    }

    /// <summary>Finds an action by id.</summary>
    public ActionSpec? Action(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        foreach (ActionSpec action in Actions)
        {
            if (string.Equals(action.Id, id, StringComparison.Ordinal))
            {
                return action;
            }
        }
        return null;
    }

    /// <summary>Finds a panel by id.</summary>
    public PanelSpec? Panel(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        foreach (PanelSpec panel in Panels)
        {
            if (string.Equals(panel.Id, id, StringComparison.Ordinal))
            {
                return panel;
            }
        }
        return null;
    }

    /// <summary>Finds a component by id.</summary>
    public ComponentSpec? Component(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        foreach (ComponentSpec component in Components)
        {
            if (string.Equals(component.Id, id, StringComparison.Ordinal))
            {
                return component;
            }
        }
        return null;
    }

    /// <summary>
    /// The live view of one slot for one context: the items whose permission the context
    /// holds and whose <c>visible_if</c> is true, in render order, re-evaluated whenever
    /// the context (the draft) changes.
    /// </summary>
    /// <param name="slot">The slot's full name.</param>
    /// <param name="context">The values the expressions read.</param>
    /// <param name="doctype">The DocType whose items to show, for a <c>doc.*</c> or <c>list.*</c> slot.</param>
    /// <param name="services">The dispatcher, shell services and query runner a click or a data block uses.</param>
    public SlotView ForSlot(string slot, ExtensionContext context, string? doctype = null, ExtensionServices? services = null)
    {
        ArgumentNullException.ThrowIfNull(slot);
        ArgumentNullException.ThrowIfNull(context);
        return new SlotView(this, slot, context, doctype, services ?? ExtensionServices.None);
    }

    /// <summary>The items a slot holds, in render order, before permission and visibility filtering.</summary>
    internal IReadOnlyList<IUiItem> ItemsOf(string slot, string? doctype)
    {
        if (!_slots.TryGetValue(slot, out List<IUiItem>? items))
        {
            return [];
        }
        if (doctype is null)
        {
            return items;
        }
        return [.. items.Where(item => item.Doctype is null || string.Equals(item.Doctype, doctype, StringComparison.Ordinal))];
    }

    private IEnumerable<IUiItem> Enumerate()
    {
        foreach (ActionSpec action in Actions)
        {
            yield return action;
        }
        foreach (PanelSpec panel in Panels)
        {
            yield return panel;
        }
        foreach (TileSpec tile in Tiles)
        {
            yield return tile;
        }
        foreach (TabSpec tab in Tabs)
        {
            yield return tab;
        }
    }

    private static int CompareItems(IUiItem left, IUiItem right)
    {
        int byOrder = left.Order.CompareTo(right.Order);
        if (byOrder != 0)
        {
            return byOrder;
        }
        int byApp = string.CompareOrdinal(left.App, right.App);
        return byApp != 0 ? byApp : string.CompareOrdinal(left.Id, right.Id);
    }

    private static ExtensionSet Disabled(DiagnosticSink sink, string version, string? requiresShell, string? shellContract) =>
        new(version, requiresShell, shellContract, false, false, sink, [], [], [], [], []);

    private static ExtensionSet CreateEmpty()
    {
        ExtensionSet empty = new(ExtensionLimits.ContractVersion, null, ExtensionLimits.ShellContract, true, false,
            new DiagnosticSink(), [], [], [], [], []);
        return empty;
    }
}
