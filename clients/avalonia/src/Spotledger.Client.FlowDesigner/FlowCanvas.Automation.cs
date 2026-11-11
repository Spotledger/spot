using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Controls;

namespace Spotledger.Client.FlowDesigner;

/// The automation half of the canvas: a pane whose children are every node and every connection,
/// each with a name a screen reader can read (the kind and id for a node, both ends and the
/// condition for an edge) and a stable automation id derived from the node id. Nodes carry the
/// selection-item and invoke patterns, so a keyboard-only or assistive-technology user can select
/// and focus them without the pointer.
public partial class FlowCanvas
{

    private FlowCanvasAutomationPeer? _peer;

    protected override AutomationPeer OnCreateAutomationPeer() => PeerShim();

    internal FlowCanvasAutomationPeer PeerShim() => _peer ??= new FlowCanvasAutomationPeer(this);

    internal IReadOnlyList<AutomationPeer> BuildChildPeers()
    {
        List<AutomationPeer> peers = [];
        if (Document is null)
        {
            return peers;
        }

        foreach (FlowNode node in Document.Nodes)
        {
            peers.Add(new FlowNodeAutomationPeer(this, node));
        }

        foreach (FlowEdge edge in Document.Edges)
        {
            peers.Add(new FlowEdgeAutomationPeer(this, edge));
        }

        return peers;
    }

    internal void SelectFromPeer(string id)
    {
        Selection.SelectOnly(id);
        FocusedNodeId = id;
        OnSelectionChanged();
    }

    internal void AddFromPeer(string id)
    {
        Selection.Add(id);
        FocusedNodeId = id;
        OnSelectionChanged();
    }

    internal void RemoveFromPeer(string id)
    {
        if (Selection.Contains(id))
        {
            Selection.Toggle(id);
            OnSelectionChanged();
        }
    }

    internal void InvokeFromPeer(string id) => FocusNode(id);

    internal void ToggleEdgeFromPeer(FlowEdgeKey key)
    {
        Selection.ToggleEdge(key);
        OnSelectionChanged();
    }
}

/// The canvas peer: a pane that lists the nodes and connections as its children.
public sealed class FlowCanvasAutomationPeer : ControlAutomationPeer, ISelectionProvider
{

    private readonly FlowCanvas _owner;

    public FlowCanvasAutomationPeer(FlowCanvas owner)
        : base(owner) => _owner = owner;

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Pane;

    protected override string GetClassNameCore() => nameof(FlowCanvas);

    protected override string GetNameCore() => FlowStrings.CanvasName;

    protected override string? GetHelpTextCore() =>
        _owner.Document is { Nodes.Count: > 0 } ? FlowStrings.PaletteHint : FlowStrings.EmptyCanvas;

    protected override IReadOnlyList<AutomationPeer>? GetChildrenCore() => _owner.BuildChildPeers();

    protected override bool IsControlElementCore() => true;

    public bool CanSelectMultiple => true;

    public bool IsSelectionRequired => false;

    public IReadOnlyList<AutomationPeer> GetSelection()
    {
        List<AutomationPeer> peers = [];
        foreach (AutomationPeer child in _owner.BuildChildPeers())
        {
            if (child is ISelectionItemProvider item && item.IsSelected)
            {
                peers.Add(child);
            }
        }

        return peers;
    }
}

/// Peer for one node: its kind and id in the name, its problems in the help text.
public sealed class FlowNodeAutomationPeer : ControlAutomationPeer, ISelectionItemProvider, IInvokeProvider
{

    private readonly FlowCanvas _owner;

    private readonly FlowNode _node;

    public FlowNodeAutomationPeer(FlowCanvas owner, FlowNode node)
        : base(owner)
    {
        _owner = owner;
        _node = node;
    }

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.ListItem;

    protected override string GetClassNameCore() => nameof(FlowNode);

    protected override string GetNameCore() =>
        FlowStrings.NodeName(_owner.Catalogue.DisplayName(_node.Kind), _node.Id);

    protected override string? GetHelpTextCore() => FlowStrings.NodeHelp(
        _owner.Catalogue.DisplayName(_node.Kind),
        _node.Id,
        _owner.Validation?.ForNode(_node.Id).Count ?? 0);

    protected override string GetAutomationIdCore() => "flow-node-" + _node.Id;

    protected override bool IsControlElementCore() => true;

    protected override bool IsContentElementCore() => true;

    public bool IsSelected => _owner.Selection.Contains(_node.Id);

    public ISelectionProvider? SelectionContainer => _owner.PeerShim();

    public void Select() => _owner.SelectFromPeer(_node.Id);

    public void AddToSelection() => _owner.AddFromPeer(_node.Id);

    public void RemoveFromSelection() => _owner.RemoveFromPeer(_node.Id);

    public void Invoke() => _owner.InvokeFromPeer(_node.Id);
}

/// Peer for one connection: both ends in the name, the condition in the help text.
public sealed class FlowEdgeAutomationPeer : ControlAutomationPeer, ISelectionItemProvider
{

    private readonly FlowCanvas _owner;

    private readonly FlowEdge _edge;

    public FlowEdgeAutomationPeer(FlowCanvas owner, FlowEdge edge)
        : base(owner)
    {
        _owner = owner;
        _edge = edge;
    }

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.ListItem;

    protected override string GetClassNameCore() => nameof(FlowEdge);

    protected override string GetNameCore() => FlowStrings.EdgeName(_edge.From, _edge.To);

    protected override string? GetHelpTextCore() => FlowStrings.EdgeHelp(_edge.From, _edge.To, _edge.Condition);

    protected override string GetAutomationIdCore() => "flow-edge-" + _edge.From + "-" + _edge.To;

    protected override bool IsControlElementCore() => true;

    protected override bool IsContentElementCore() => true;

    public bool IsSelected => _owner.Selection.ContainsEdge(_edge.Key);

    public ISelectionProvider? SelectionContainer => _owner.PeerShim();

    public void Select() => _owner.ToggleEdgeFromPeer(_edge.Key);

    public void AddToSelection() => _owner.ToggleEdgeFromPeer(_edge.Key);

    public void RemoveFromSelection()
    {
        if (_owner.Selection.ContainsEdge(_edge.Key))
        {
            _owner.ToggleEdgeFromPeer(_edge.Key);
        }
    }
}
