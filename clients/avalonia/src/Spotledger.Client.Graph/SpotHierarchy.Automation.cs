using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Controls;

namespace Spotledger.Client.Graph;

/// <summary>Automation half of <see cref="SpotHierarchy"/>: a tree peer whose children are
/// the visible rows (or the whole set under test), and row peers that carry a stable
/// automation id, the level in their name, the expand/collapse pattern and the selection-item
/// pattern. Whole-set counts are not needed here because a tree is a hierarchy, not a list.</summary>
public partial class SpotHierarchy
{
    internal static bool AutomationForWholeSet { get; set; }

    internal SpotHierarchyAutomationPeer? CachedPeer { get; private set; }

    /// <inheritdoc />
    protected override AutomationPeer OnCreateAutomationPeer() => new SpotHierarchyAutomationPeer(this);

    internal AutomationPeer OnCreateAutomationPeerShim()
        => CachedPeer ??= new SpotHierarchyAutomationPeer(this);

    internal void ExpandFromPeer(GraphNodeKey key)
    {
        if (_model is { } model)
        {
            Start(token => model.ExpandAsync(key, token));
            InvalidateVisual();
        }
    }

    internal void CollapseFromPeer(GraphNodeKey key)
    {
        _model?.Collapse(key);
        InvalidateVisual();
    }

    internal void SelectFromPeer(GraphNodeKey key)
    {
        if (_model is { } model)
        {
            model.Selection.Select(key);
            model.MoveCursorTo(key);
            NotifySelectionChanged();
            InvalidateVisual();
        }
    }

    internal void ToggleFromPeer(GraphNodeKey key)
    {
        if (_model is { } model)
        {
            model.Selection.Toggle(key);
            NotifySelectionChanged();
            InvalidateVisual();
        }
    }

    internal IReadOnlyList<HierarchyRow> PeerRows()
    {
        if (_model is not { } model)
        {
            return [];
        }

        var rows = model.Rows;
        if (AutomationForWholeSet)
        {
            return rows;
        }

        var first = Math.Max(0, FirstVisibleRow);
        var visible = Math.Max(1, VisibleRowCount());
        var buffer = new List<HierarchyRow>(visible);
        for (var i = first; i < rows.Count && i < first + visible; i++)
        {
            buffer.Add(rows[i]);
        }

        return buffer;
    }
}

/// <summary>The tree peer: tree control type, the selection pattern over node keys, and the
/// visible rows as children.</summary>
public sealed class SpotHierarchyAutomationPeer : ControlAutomationPeer, ISelectionProvider
{
    private readonly SpotHierarchy _owner;

    /// <summary>Creates the peer over the tree.</summary>
    /// <param name="owner">The tree.</param>
    public SpotHierarchyAutomationPeer(SpotHierarchy owner) : base(owner) => _owner = owner;

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Tree;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(SpotHierarchy);

    /// <inheritdoc />
    protected override string? GetNameCore() => "Spotledger hierarchy";

    /// <inheritdoc />
    protected override string? GetAutomationIdCore() => "graph-hierarchy";

    /// <inheritdoc />
    protected override bool IsControlElementCore() => true;

    /// <inheritdoc />
    protected override bool IsContentElementCore() => true;

    /// <inheritdoc />
    protected override IReadOnlyList<AutomationPeer>? GetChildrenCore()
    {
        var rows = _owner.PeerRows();
        var peers = new List<AutomationPeer>(rows.Count);
        foreach (var row in rows)
        {
            peers.Add(new SpotHierarchyRowAutomationPeer(_owner, row));
        }

        return peers;
    }

    /// <inheritdoc />
    public bool CanSelectMultiple => true;

    /// <inheritdoc />
    public bool IsSelectionRequired => false;

    /// <inheritdoc />
    public IReadOnlyList<AutomationPeer> GetSelection()
    {
        var peers = new List<AutomationPeer>();
        if (_owner.Model is not { } model)
        {
            return peers;
        }

        foreach (var row in _owner.PeerRows())
        {
            if (model.Selection.Contains(row.Key))
            {
                peers.Add(new SpotHierarchyRowAutomationPeer(_owner, row));
            }
        }

        return peers;
    }
}

/// <summary>Peer for one visible row: its level in the name, a key-derived stable automation
/// id, the expand/collapse pattern and the selection-item pattern. A locked row's name is the
/// locked placeholder — never the title the caller may not read.</summary>
public sealed class SpotHierarchyRowAutomationPeer : ControlAutomationPeer, IExpandCollapseProvider, ISelectionItemProvider
{
    private readonly SpotHierarchy _owner;
    private readonly HierarchyRow _row;

    /// <summary>Creates the row peer.</summary>
    /// <param name="owner">The tree.</param>
    /// <param name="row">The row.</param>
    public SpotHierarchyRowAutomationPeer(SpotHierarchy owner, HierarchyRow row) : base(owner)
    {
        _owner = owner;
        _row = row;
    }

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.TreeItem;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(HierarchyRow);

    /// <inheritdoc />
    protected override string? GetNameCore()
    {
        var level = _row.Depth + 1;
        if (_row.Node.IsLocked)
        {
            return $"Locked item, level {level}";
        }

        var state = !_row.Node.HasChildren ? string.Empty : _row.IsExpanded ? ", expanded" : ", collapsed";
        return $"{_row.Title}, level {level}{state}";
    }

    /// <inheritdoc />
    protected override string? GetAutomationIdCore()
        => string.Concat("graph-hierarchy-row-", _row.Key.ToString().Replace('/', '-').Replace('.', '-'));

    /// <inheritdoc />
    protected override bool IsControlElementCore() => true;

    /// <inheritdoc />
    protected override bool IsContentElementCore() => true;

    // ---- IExpandCollapseProvider -------------------------------------------------

    /// <inheritdoc />
    public ExpandCollapseState ExpandCollapseState
        => !_row.Node.HasChildren
            ? ExpandCollapseState.LeafNode
            : _row.IsExpanded ? ExpandCollapseState.Expanded : ExpandCollapseState.Collapsed;

    /// <inheritdoc />
    public void Expand() => _owner.ExpandFromPeer(_row.Key);

    /// <inheritdoc />
    public void Collapse() => _owner.CollapseFromPeer(_row.Key);

    /// <inheritdoc />
    public bool ShowsMenu => false;

    // ---- ISelectionItemProvider ---------------------------------------------------

    /// <inheritdoc />
    public bool IsSelected => _owner.Model?.Selection.Contains(_row.Key) == true;

    /// <inheritdoc />
    public void Select() => _owner.SelectFromPeer(_row.Key);

    /// <inheritdoc />
    public void AddToSelection() => _owner.ToggleFromPeer(_row.Key);

    /// <inheritdoc />
    public void RemoveFromSelection()
    {
        if (_owner.Model is { } model && model.Selection.Contains(_row.Key))
        {
            model.Selection.Toggle(_row.Key);
            _owner.NotifySelectionChanged();
        }
    }

    /// <inheritdoc />
    public ISelectionProvider? SelectionContainer => _owner.OnCreateAutomationPeerShim() as ISelectionProvider;
}
