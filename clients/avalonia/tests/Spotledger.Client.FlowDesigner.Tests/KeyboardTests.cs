using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// Keyboard-only editing is complete: a node is added by type-ahead, a connection is made with the
/// C key and the arrows, nodes move with the arrow keys, Delete deletes, Tab walks the panels in a
/// fixed order, and every node and edge has an accessible name and a stable automation id.
public sealed class KeyboardTests
{
    [AvaloniaFact]

    public void TypeAheadFindsAKindAndEnterAddsIt()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        int before = canvas.Document!.Nodes.Count;

        Assert.True(canvas.HandleKey(Key.A, KeyModifiers.None));
        Assert.True(canvas.IsTypeAheadOpen);
        canvas.TypeTypeAhead("decis");

        Assert.Equal("decision", canvas.TypeAheadMatches[0].Kind);
        Assert.True(canvas.CommitTypeAhead());

        Assert.Equal(before + 1, canvas.Document.Nodes.Count);
        Assert.Equal("decision", canvas.Document.Nodes[^1].Kind);
        Assert.False(canvas.IsTypeAheadOpen);
    }

    [AvaloniaFact]

    public void TypeAheadBackspaceAndEscapeLeaveTheDocumentAlone()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        int before = canvas.Document!.Nodes.Count;
        canvas.BeginTypeAhead();
        canvas.TypeTypeAhead("inte");
        canvas.HandleKey(Key.Back, KeyModifiers.None);

        Assert.Equal("int", canvas.TypeAheadQuery);

        canvas.HandleKey(Key.Escape, KeyModifiers.None);
        Assert.Equal(string.Empty, canvas.TypeAheadQuery);
        Assert.False(canvas.IsTypeAheadOpen);
        Assert.Equal(before, canvas.Document.Nodes.Count);
    }

    [AvaloniaFact]

    public void AConnectionIsMadeWithTheKeyboardAlone()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        canvas.Select("start");

        Assert.True(canvas.HandleKey(Key.C, KeyModifiers.None));
        Assert.Equal("start", canvas.ConnectFrom);
        Assert.NotNull(canvas.ConnectCandidate);

        while (canvas.ConnectCandidate != "notify")
        {
            Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        }

        Assert.True(canvas.HandleKey(Key.Enter, KeyModifiers.None));

        Assert.Contains(canvas.Document!.Edges, edge => edge.Key == new FlowEdgeKey("start", "notify"));
        Assert.Null(canvas.ConnectFrom);
    }

    [AvaloniaFact]

    public void EscapeCancelsAKeyboardConnection()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        canvas.Select("start");
        canvas.HandleKey(Key.C, KeyModifiers.None);

        Assert.True(canvas.HandleKey(Key.Escape, KeyModifiers.None));

        Assert.Null(canvas.ConnectFrom);
        Assert.Null(canvas.ConnectCandidate);
    }

    [AvaloniaFact]

    public void ArrowsMoveTheSelectionOnTheGridAndControlArrowsNudgeByOne()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        canvas.Select("start");
        FlowPoint start = canvas.Document!.Node("start")!.Position;

        Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        Assert.Equal(start.Offset(FlowLayoutEngine.GridSize, 0), canvas.Document.Node("start")!.Position);

        Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.Control));
        Assert.Equal(start.Offset(FlowLayoutEngine.GridSize + 1, 0), canvas.Document.Node("start")!.Position);

        Assert.True(canvas.HandleKey(Key.Down, KeyModifiers.None));
        Assert.Equal(start.Offset(FlowLayoutEngine.GridSize + 1, FlowLayoutEngine.GridSize), canvas.Document.Node("start")!.Position);
    }

    [AvaloniaFact]

    public void ArrowsWithoutASelectionWalkTheNodeFocus()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));

        Assert.Equal("start", canvas.Selection.SingleNodeId);

        canvas.ClearSelection();
        Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        Assert.Equal("create_delivery", canvas.Selection.SingleNodeId);
    }

    [AvaloniaFact]

    public void DeleteRemovesTheSelectionAsOneStepAndControlZPutsItBack()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        string before = canvas.Document!.ToText();
        canvas.Select("approve");

        Assert.True(canvas.HandleKey(Key.Delete, KeyModifiers.None));
        Assert.Null(canvas.Document.Node("approve"));
        Assert.Equal(2, canvas.Document.Edges.Count);

        Assert.True(canvas.HandleKey(Key.Z, KeyModifiers.Control));
        Assert.Equal(before, canvas.Document.ToText(), StringComparer.Ordinal);
    }

    [AvaloniaFact]

    public void TabWalksThePanelRingInBothDirectionsAndAnnouncesTheMove()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        List<string> visits = [];
        canvas.FocusRequested += (_, e) => visits.Add(e.Panel);

        Assert.Equal("canvas", canvas.FocusedPanel);
        canvas.HandleKey(Key.Tab, KeyModifiers.None);
        canvas.HandleKey(Key.Tab, KeyModifiers.None);
        canvas.HandleKey(Key.Tab, KeyModifiers.None);

        Assert.Equal(["palette", "properties", "problems"], visits);
        Assert.Equal("problems", canvas.FocusedPanel);

        canvas.HandleKey(Key.Tab, KeyModifiers.None);
        Assert.Equal("canvas", canvas.FocusedPanel);

        canvas.HandleKey(Key.Tab, KeyModifiers.Shift);
        Assert.Equal("problems", canvas.FocusedPanel);
        Assert.Equal(
            ["canvas", "palette", "properties", "problems"],
            FlowCanvas.PanelOrder.ToArray());
    }

    [AvaloniaFact]

    public void EnterOnASelectionAsksForThePropertyPanel()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        canvas.Select("approve");
        string? asked = null;
        canvas.FocusRequested += (_, e) => asked = e.Panel;

        Assert.True(canvas.HandleKey(Key.Enter, KeyModifiers.None));

        Assert.Equal("properties", asked);
        Assert.Equal("approve", canvas.Selection.SingleNodeId);
    }

    [AvaloniaFact]

    public void EveryNodeAndEdgeHasANameAndAStableAutomationId()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        AutomationPeer peer = canvas.PeerShim();

        Assert.Equal(AutomationControlType.Pane, peer.GetAutomationControlType());
        Assert.Equal(FlowStrings.CanvasName, peer.GetName());
        Assert.Equal("flow-canvas", peer.GetAutomationId());

        IReadOnlyList<AutomationPeer> children = peer.GetChildren()!;
        Assert.Equal(canvas.Document!.Nodes.Count + canvas.Document.Edges.Count, children.Count);
        Assert.All(children, child => Assert.False(string.IsNullOrWhiteSpace(child.GetName())));

        FlowNodeAutomationPeer node = Assert.IsType<FlowNodeAutomationPeer>(children.First(child => child.GetAutomationId() == "flow-node-create_delivery"));
        Assert.Equal("Document node create_delivery", node.GetName());
        Assert.Equal("flow-node-create_delivery", node.GetAutomationId());

        FlowEdgeAutomationPeer edge = Assert.IsType<FlowEdgeAutomationPeer>(
            children.First(child => child.GetAutomationId() == "flow-edge-approve-notify"));
        Assert.Equal("Connection from approve to notify", edge.GetName());

        FlowCanvas escalation = FlowFixtures.Canvas(FlowFixtures.Document("approval_escalation.toml"));
        FlowEdgeAutomationPeer conditional = Assert.IsType<FlowEdgeAutomationPeer>(
            escalation.PeerShim().GetChildren()!.First(child => child.GetAutomationId() == "flow-edge-first-approved"));
        Assert.Contains(" when ", conditional.GetHelpText()!, StringComparison.Ordinal);
    }

    [AvaloniaFact]

    public void ANodePeerSelectsAndInvokesItsNode()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        AutomationPeer peer = canvas.PeerShim();
        FlowNodeAutomationPeer node = (FlowNodeAutomationPeer)peer.GetChildren()!
            .First(child => child.GetAutomationId() == "flow-node-hold");

        Assert.False(node.IsSelected);
        node.Select();
        Assert.True(node.IsSelected);
        Assert.Equal("hold", canvas.Selection.SingleNodeId);

        node.RemoveFromSelection();
        Assert.False(node.IsSelected);

        ((IInvokeProvider)node).Invoke();
        Assert.Equal("hold", canvas.FocusedNodeId);
    }
}
