using System;
using System.Collections.Generic;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The auto-layout: deterministic (the same document always gives the same points, which is what
/// the golden texts pin), layered left to right with every edge pointing forward, snapped to the
/// grid, and undoable as one step. A cycle does not hang the algorithm: its nodes land in one extra
/// column and are reported so the problem list can point at them.
public sealed class AutoLayoutTests
{

    private static readonly Dictionary<string, string> Goldens = new(StringComparer.Ordinal)
    {
        ["approval_escalation.toml"] = Join(
            "approved 3 864,48",
            "escalate 2 592,48",
            "first 1 320,48",
            "rejected 3 864,160",
            "start 0 48,48",
            "cyclic: "),
        ["document_then_notify.toml"] = Join(
            "create_invoice 1 320,48",
            "notify 3 864,48",
            "post_invoice 2 592,48",
            "start 0 48,48",
            "cyclic: "),
        ["integration_retry.toml"] = Join(
            "call 1 320,48",
            "done 2 592,48",
            "failed 0 48,160",
            "start 0 48,48",
            "cyclic: "),
        ["order_to_cash.toml"] = Join(
            "approve 3 864,48",
            "create_delivery 1 320,48",
            "hold 4 1136,160",
            "notify 4 1136,48",
            "post_delivery 2 592,48",
            "start 0 48,48",
            "cyclic: "),
    };
    /// The four templates the server ships.
    public static TheoryData<string> Templates => new()
    {
        "approval_escalation.toml",
        "document_then_notify.toml",
        "integration_retry.toml",
        "order_to_cash.toml",
    };

    [Theory]
    [MemberData(nameof(Templates))]

    public void TheLayoutOfATemplateMatchesItsGolden(string template)
    {
        FlowLayoutResult layout = FlowLayoutEngine.Compute(FlowFixtures.Document(template));

        Assert.Equal(Goldens[template], FlowLayoutEngine.Describe(layout));
    }

    [Theory]
    [MemberData(nameof(Templates))]

    public void TheLayoutIsDeterministic(string template)
    {
        FlowDocument document = FlowFixtures.Document(template);
        FlowLayoutResult first = FlowLayoutEngine.Compute(document);
        FlowLayoutResult second = FlowLayoutEngine.Compute(FlowDocument.FromText(document.ToText()));

        Assert.Equal(FlowLayoutEngine.Describe(first), FlowLayoutEngine.Describe(second));
    }

    [Theory]
    [MemberData(nameof(Templates))]

    public void EveryEdgePointsForwardAndEveryPointSitsOnTheGrid(string template)
    {
        FlowDocument document = FlowFixtures.Document(template);
        FlowLayoutResult layout = FlowLayoutEngine.Compute(document);

        Assert.Equal(document.Nodes.Count, layout.Nodes.Count);
        Assert.Empty(layout.CyclicNodes);
        foreach (FlowLayoutNode placement in layout.Nodes)
        {
            Assert.Equal(0d, placement.Position.X % FlowLayoutEngine.GridSize);
            Assert.Equal(0d, placement.Position.Y % FlowLayoutEngine.GridSize);
        }

        foreach (FlowEdge edge in document.Edges)
        {
            Assert.True(layout.Node(edge.From)!.Layer < layout.Node(edge.To)!.Layer,
                "the edge " + edge.From + " to " + edge.To + " does not point forward");
        }
    }

    [Fact]

    public void ACycleIsReportedInsteadOfHangingTheLayouter()
    {
        FlowDocument document = FlowDocument.FromText(TestHost.Golden("document_then_notify.toml.golden"));
        document.AddEdge(new FlowEdge("notify", "start"));

        FlowLayoutResult layout = FlowLayoutEngine.Compute(document);

        Assert.NotEmpty(layout.CyclicNodes);
        Assert.Contains("start", layout.CyclicNodes, StringComparer.Ordinal);
        Assert.Equal(document.Nodes.Count, layout.Nodes.Count);
        Assert.All(layout.Nodes, placement => Assert.True(placement.Position.X >= FlowLayoutEngine.Origin));
    }

    [Fact]

    public void ApplyingAndUndoingTheLayoutRestoresThePreviousPoints()
    {
        FlowDocument document = FlowFixtures.Document("order_to_cash.toml");
        foreach (FlowNode node in document.Nodes)
        {
            node.Position = new FlowPoint(1000, 1000);
        }

        FlowEditor editor = new(document);
        AutoLayoutCommand command = new(document);
        Assert.True(editor.Execute(command));
        Assert.NotEqual(new FlowPoint(1000, 1000), document.Node("start")!.Position);

        Assert.True(editor.Undo());
        Assert.All(document.Nodes, node => Assert.Equal(new FlowPoint(1000, 1000), node.Position));

        Assert.True(editor.Redo());
        Assert.NotEqual(new FlowPoint(1000, 1000), document.Node("start")!.Position);
    }

    [Fact]

    public void AnEmptyDocumentLaysOutToNothingWithoutDividingByZero()
    {
        FlowLayoutResult layout = FlowLayoutEngine.Compute(new FlowDocument("empty", "demo"));

        Assert.Empty(layout.Nodes);
        Assert.Empty(layout.CyclicNodes);
        Assert.True(layout.Width > 0);
        Assert.True(layout.Height > 0);
    }

    private static string Join(params string[] lines) => string.Join('\n', lines) + "\n";
}
