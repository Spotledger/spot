using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// Backward edges typed Undo or Rewind, and the shaded region of a commit point.
public sealed class StructureTests
{

    private const string Loop =
        """
        name = "loop"
        app = "a"
        version = 1
        status = "draft"

        [[nodes]]
        id = "start"
        kind = "form"
        [[nodes]]
        id = "post"
        kind = "document"
        [[nodes]]
        id = "check"
        kind = "form"
        [[nodes]]
        id = "done"
        kind = "commit"

        [[edges]]
        from = "start"
        to = "post"
        [[edges]]
        from = "post"
        to = "check"
        [[edges]]
        from = "check"
        to = "start"
        [[edges]]
        from = "check"
        to = "done"
        """;

    [Fact]
    public void ABackwardEdgeIsFoundByAWalkFromTheEntry()
    {
        FlowDocument document = FlowDocument.FromText(Loop);

        FlowEdge back = Assert.Single(EdgeModes.Backward(document));

        Assert.Equal(new FlowEdgeKey("check", "start"), back.Key);
    }

    [Fact]
    public void ASelfLoopIsBackward()
    {
        FlowDocument document = FlowDocument.FromText(Loop.Replace("from = \"check\"\nto = \"done\"", "from = \"check\"\nto = \"check\"", System.StringComparison.Ordinal));

        Assert.Contains(EdgeModes.Backward(document), edge => edge.Key == new FlowEdgeKey("check", "check"));
    }

    [Fact]
    public void AnUntypedBackwardEdgeGetsAHintAndATypedOneDoesNot()
    {
        FlowDocument document = FlowDocument.FromText(Loop);
        FlowEditor editor = new(document);

        FlowProblem hint = Assert.Single(EdgeModes.Hints(document));
        Assert.Equal("back_edge_untyped", hint.Code);
        Assert.Equal(FlowProblemSeverity.Hint, hint.Severity);
        Assert.Equal(new FlowEdgeKey("check", "start"), hint.Edge);

        IFlowCommand command = EdgeModes.SetMode(document, new FlowEdgeKey("check", "start"), EdgeMode.Undo)!;
        Assert.True(editor.Execute(command));

        Assert.Equal(EdgeMode.Undo, EdgeModes.Of(document.Edges.Single(e => e.To == "start" && e.From == "check")));
        Assert.Empty(EdgeModes.Hints(document));
        Assert.Contains("label = \"undo\"", document.ToText(), System.StringComparison.Ordinal);

        Assert.True(editor.Undo());
        Assert.Single(EdgeModes.Hints(document));
    }

    [Fact]
    public void RewindIsTheOtherMarkAndAForwardEdgeCarryingAModeIsHinted()
    {
        FlowDocument document = FlowDocument.FromText(Loop);
        FlowEditor editor = new(document);

        Assert.True(editor.Execute(EdgeModes.SetMode(document, new FlowEdgeKey("check", "start"), EdgeMode.Rewind)!));
        Assert.Equal(EdgeMode.Rewind, EdgeModes.Of(document.Edges.Single(e => e.From == "check" && e.To == "start")));

        Assert.True(editor.Execute(EdgeModes.SetMode(document, new FlowEdgeKey("start", "post"), EdgeMode.Undo)!));
        FlowProblem hint = Assert.Single(EdgeModes.Hints(document));
        Assert.Equal("mode_on_forward_edge", hint.Code);
    }

    [Fact]
    public void AnEdgeLabelledWithAPersonsOwnWordsIsNotOverwritten()
    {
        FlowDocument document = FlowDocument.FromText(Loop);
        FlowEditor editor = new(document);
        Assert.True(editor.Execute(new SetEdgeFieldCommand(new FlowEdgeKey("check", "start"), "label", "retry")));

        Assert.Null(EdgeModes.SetMode(document, new FlowEdgeKey("check", "start"), EdgeMode.Undo));
        Assert.Null(EdgeModes.SetMode(document, new FlowEdgeKey("start", "nowhere"), EdgeMode.Undo));
    }

    [Fact]
    public void TheRegionOfACommitPointIsEveryStepBeforeItAndNotItself()
    {
        FlowDocument document = FlowDocument.FromText(Loop);

        var regions = CommitRegions.Of(document);

        Assert.Equal(["check", "post", "start"], regions["done"].OrderBy(id => id, System.StringComparer.Ordinal));
        Assert.DoesNotContain("done", regions["done"]);
    }

    [Fact]
    public void ARegionStopsAtAnEarlierCommitPoint()
    {
        FlowDocument document = FlowDocument.FromText(
            """
            name = "two"
            app = "a"
            version = 1
            status = "draft"

            [[nodes]]
            id = "a"
            kind = "form"
            [[nodes]]
            id = "c1"
            kind = "commit"
            [[nodes]]
            id = "b"
            kind = "form"
            [[nodes]]
            id = "c2"
            kind = "commit"

            [[edges]]
            from = "a"
            to = "c1"
            [[edges]]
            from = "c1"
            to = "b"
            [[edges]]
            from = "b"
            to = "c2"
            """);

        var regions = CommitRegions.Of(document);

        Assert.Equal(["a"], regions["c1"].ToArray());
        Assert.Equal(["b"], regions["c2"].ToArray());
        Assert.Equal(["a", "b"], CommitRegions.Held(document).OrderBy(id => id, System.StringComparer.Ordinal));
    }

    [Fact]
    public void AProcessWithNoCommitPointHoldsNothing()
    {
        FlowDocument document = FlowDocument.FromText(Loop.Replace("kind = \"commit\"", "kind = \"form\"", System.StringComparison.Ordinal));

        Assert.Empty(CommitRegions.Of(document));
        Assert.Empty(CommitRegions.Held(document));
    }

    [Fact]
    public void AnUndoEdgeDoesNotMakeAStepPartOfARegion()
    {
        FlowDocument document = FlowDocument.FromText(Loop);
        FlowEditor editor = new(document);
        Assert.True(editor.Execute(EdgeModes.SetMode(document, new FlowEdgeKey("check", "start"), EdgeMode.Undo)!));

        Assert.Equal(3, CommitRegions.Of(document)["done"].Count);
    }

    [Fact]
    public void ADeepChainDoesNotOverflowTheStack()
    {
        System.Text.StringBuilder text = new("name = \"deep\"\napp = \"a\"\nversion = 1\nstatus = \"draft\"\n");
        const int Count = 900;
        for (int i = 0; i < Count; i++)
        {
            text.Append("\n[[nodes]]\nid = \"n").Append(i).Append("\"\nkind = \"form\"\n");
        }

        for (int i = 0; i < Count - 1; i++)
        {
            text.Append("\n[[edges]]\nfrom = \"n").Append(i).Append("\"\nto = \"n").Append(i + 1).Append("\"\n");
        }

        text.Append("\n[[edges]]\nfrom = \"n").Append(Count - 1).Append("\"\nto = \"n0\"\n");
        FlowDocument document = FlowDocument.FromText(text.ToString());

        FlowEdge back = Assert.Single(EdgeModes.Backward(document));

        Assert.Equal("n0", back.To);
    }
}
