using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia;
using Avalonia.Headless.XUnit;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The canvas itself: pan and zoom keep the anchor still and clamp, hit testing maps a click to the
/// node under it and to its ports, an add lands on the grid with a free id, multi-select and
/// copy/paste work, a connection reports live validity before it is made, and the minimap projects
/// both ways so a click in the overview moves the canvas.
public sealed class CanvasTests
{
    [AvaloniaFact]

    public void PanningMovesTheDrawingAndZoomKeepsTheAnchorStill()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        Point anchor = new(300, 200);
        FlowPoint worldBefore = canvas.ToWorld(anchor);

        canvas.SetZoom(1.6, anchor);

        FlowPoint worldAfter = canvas.ToWorld(anchor);
        Assert.Equal(worldBefore.X, worldAfter.X, 3);
        Assert.Equal(worldBefore.Y, worldAfter.Y, 3);

        canvas.SetZoom(50, anchor);
        Assert.Equal(3, canvas.Zoom);
        canvas.SetZoom(0.01, anchor);
        Assert.Equal(0.25, canvas.Zoom);

        double panX = canvas.PanX;
        canvas.PanBy(25, -10);
        Assert.Equal(panX + 25, canvas.PanX);
    }

    [AvaloniaFact]

    public void HitTestFindsTheNodeUnderThePointerAndNothingOverEmptyCanvas()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        FlowNode? hit = canvas.HitTest(FlowFixtures.Centre(canvas, "create_delivery"));

        Assert.NotNull(hit);
        Assert.Equal("create_delivery", hit!.Id);
        Assert.Null(canvas.HitTest(new Point(2, 2)));
    }

    [AvaloniaFact]

    public void PortsAreDistinguishedOnTheRightAndLeftEdge()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        FlowNode node = canvas.Document!.Node("post_delivery")!;

        FlowNode? output = canvas.PortHit(canvas.ToScreen(FlowCanvas.OutputPort(node)), out bool fromPort);
        FlowNode? input = canvas.PortHit(canvas.ToScreen(FlowCanvas.InputPort(node)), out bool toPort);

        Assert.Equal(node.Id, output!.Id);
        Assert.True(fromPort);
        Assert.Equal(node.Id, input!.Id);
        Assert.False(toPort);
    }

    [AvaloniaFact]

    public void AddingANodeSnapsItToTheGridAndRenamesACollision()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        FlowDocument document = canvas.Document!;

        IFlowCommand? first = canvas.AddNode("decision", "extra", new FlowPoint(645, 487));
        IFlowCommand? second = canvas.AddNode("decision", "extra", new FlowPoint(645, 487));

        Assert.NotNull(first);
        Assert.NotNull(second);
        Assert.Equal(new FlowPoint(640, 480), document.Node("extra")!.Position);
        Assert.Equal(new FlowPoint(640, 480), document.Node("extra_2")!.Position);
        Assert.Equal("extra_2", canvas.Selection.SingleNodeId);
    }

    [AvaloniaFact]

    public void MultiSelectTogglesAndPrunesAfterADelete()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        Assert.True(canvas.ToggleSelection("start"));
        Assert.True(canvas.ToggleSelection("approve"));
        Assert.Equal(2, canvas.Selection.NodeIds.Count);
        Assert.False(canvas.ToggleSelection("start"));
        Assert.Single(canvas.Selection.NodeIds);

        Assert.True(canvas.DeleteSelection());
        Assert.Empty(canvas.Selection.NodeIds);
        Assert.Null(canvas.Document!.Node("approve"));
    }

    [AvaloniaFact]

    public void AConnectionIsCheckedBeforeItIsMadeAndTheRefusalsSayWhy()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        Assert.True(canvas.CheckConnection("start", "notify").Valid);
        Assert.False(canvas.CheckConnection("start", "start").Valid);
        Assert.Contains("itself", canvas.CheckConnection("start", "start").Reason, StringComparison.Ordinal);
        Assert.False(canvas.CheckConnection("start", "create_delivery").Valid);
        Assert.Contains("already connected", canvas.CheckConnection("start", "create_delivery").Reason, StringComparison.Ordinal);
        Assert.False(canvas.CheckConnection("start", "missing").Valid);
    }

    [AvaloniaFact]

    public void CompletingAConnectionAddsTheEdgeAndSelectsIt()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        string before = canvas.Document!.ToText();

        Assert.True(canvas.BeginConnect("start").Valid);
        Assert.True(canvas.CompleteConnect("notify", out string? error));
        Assert.Null(error);

        Assert.Contains(canvas.Document.Edges, edge => edge.Key == new FlowEdgeKey("start", "notify"));
        Assert.Single(canvas.Selection.EdgeKeys);
        Assert.Null(canvas.ConnectFrom);

        Assert.True(canvas.Undo());
        Assert.Equal(before, canvas.Document.ToText(), StringComparer.Ordinal);
    }

    [AvaloniaFact]

    public void TheKeyboardConnectionCandidateWalksEveryOtherNodeAndWraps()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        canvas.BeginConnect("start");
        List<string> seen = [];
        for (int i = 0; i < canvas.Document!.Nodes.Count - 1; i++)
        {
            seen.Add(canvas.ConnectCandidate!);
            canvas.NextCandidate(1);
        }

        Assert.Equal(
            canvas.Document.Nodes.Select(node => node.Id).Where(id => id != "start").ToArray(),
            seen.ToArray());
        Assert.Equal("start", canvas.ConnectFrom);
    }

    [AvaloniaFact]

    public void AReadOnlyCanvasRefusesEditsAndSaysSo()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        string before = canvas.Document!.ToText();
        canvas.IsReadOnly = true;
        canvas.Select("hold");

        Assert.Null(canvas.AddNode("document"));
        Assert.False(canvas.DeleteSelection());
        Assert.False(canvas.CheckConnection("start", "notify").Valid);
        Assert.Contains("read-only", canvas.CheckConnection("start", "notify").Reason!, StringComparison.OrdinalIgnoreCase);
        Assert.Equal(before, canvas.Document.ToText(), StringComparer.Ordinal);
        Assert.False(canvas.Undo());
    }

    [AvaloniaFact]

    public void TheMinimapProjectsBothWaysAndFramesTheViewport()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        MinimapProjection projection = canvas.Minimap!;
        FlowPoint world = new(320, 148);
        FlowPoint minimap = projection.ToMinimap(world);
        FlowPoint back = projection.ToWorld(minimap);

        Assert.Equal(world.X, back.X, 3);
        Assert.Equal(world.Y, back.Y, 3);
        (FlowPoint topLeft, FlowPoint size) = projection.ViewportFrame(canvas.PanX, canvas.PanY, canvas.Zoom, 900, 640);
        Assert.True(size.X > 0);
        Assert.True(size.Y > 0);
        Assert.True(topLeft.X >= 0);
        Assert.Equal(6, projection.Nodes(canvas.Document!).Count);
    }

    [AvaloniaFact]

    public void ClickingTheMinimapCentresTheCanvasOnThatPoint()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        Rect frame = canvas.MinimapRect;
        MinimapProjection projection = canvas.Minimap!;
        FlowPoint wanted = new(600, 300);
        FlowPoint inMinimap = projection.ToMinimap(wanted);

        canvas.ClickMinimap(new Point(frame.X + inMinimap.X, frame.Y + inMinimap.Y));

        FlowPoint centre = canvas.ToWorld(new Point(canvas.Bounds.Width / 2, canvas.Bounds.Height / 2));
        Assert.Equal(wanted.X, centre.X, 1);
        Assert.Equal(wanted.Y, centre.Y, 1);
    }

    [AvaloniaFact]

    public void ADragFromThePalettePlacesTheNodeWhereItIsDropped()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        Assert.True(canvas.BeginPaletteDrag("integration"));
        Assert.Equal("integration", canvas.PaletteDragKind);
        IFlowCommand? command = canvas.DropFromPalette(new Point(120, 240));

        Assert.NotNull(command);
        Assert.Null(canvas.PaletteDragKind);
        FlowNode placed = canvas.Document!.Nodes[^1];
        Assert.Equal("integration", placed.Kind);
        Assert.Equal(FlowLayoutEngine.Snap(canvas.ToWorld(new Point(120, 240))), placed.Position);
        Assert.False(canvas.BeginPaletteDrag("nonsense"));
    }

    [AvaloniaFact]

    public void AProblemRowFocusesItsNodeAndAnEdgeProblemSelectsTheEdge()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        FlowProblem onNode = new("bpm.x", "broken", FlowProblemSeverity.Error, "hold");
        FlowProblem onEdge = new("bpm.y", "conditional", FlowProblemSeverity.Warning, null, "approve", "notify");

        Assert.True(canvas.FocusProblem(onNode));
        Assert.Equal("hold", canvas.Selection.SingleNodeId);
        Assert.True(canvas.FocusProblem(onEdge));
        Assert.Equal(new FlowEdgeKey("approve", "notify"), Assert.Single(canvas.Selection.EdgeKeys));
        Assert.False(canvas.FocusProblem(new FlowProblem("bpm.z", "whole process", FlowProblemSeverity.Error)));
        Assert.False(canvas.FocusProblem(null));
    }

    [AvaloniaFact]

    public void FocusingANodeSelectsItAndBringsItIntoView()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();

        Assert.True(canvas.FocusNode("hold"));

        Assert.Equal("hold", canvas.Selection.SingleNodeId);
        FlowNode node = canvas.Document!.Node("hold")!;
        Point centre = canvas.ToScreen(new FlowPoint(
            node.Position.X + (FlowLayoutEngine.NodeWidth / 2),
            node.Position.Y + (FlowLayoutEngine.NodeHeight / 2)));
        Assert.InRange(centre.X, 0, canvas.Bounds.Width);
        Assert.InRange(centre.Y, 0, canvas.Bounds.Height);
        Assert.False(canvas.FocusNode("nope"));
    }

    [AvaloniaFact]

    public void AutoLayoutIsUndoableFromTheCanvas()
    {
        FlowCanvas canvas = FlowFixtures.Canvas();
        int nodes = canvas.Document!.Nodes.Count;
        foreach (FlowNode node in canvas.Document.Nodes)
        {
            node.Position = new FlowPoint(2000, 2000);
        }

        Assert.True(canvas.ApplyAutoLayout());
        Assert.NotEqual(new FlowPoint(2000, 2000), canvas.Document.Node("start")!.Position);

        Assert.True(canvas.Undo());
        Assert.All(canvas.Document.Nodes, node => Assert.Equal(new FlowPoint(2000, 2000), node.Position));
        Assert.Equal(nodes, canvas.Document.Nodes.Count);
    }
}
