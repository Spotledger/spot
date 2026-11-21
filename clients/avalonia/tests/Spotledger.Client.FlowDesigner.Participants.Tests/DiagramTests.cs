using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The state diagram generated from the order table.
public sealed class DiagramTests
{

    [Fact]
    public void EveryRuleIsAnArrowAndEveryMessageAndOutcomeAState()
    {
        StateDiagram diagram = StateDiagram.From(ParticipantFixtures.Contract());

        Assert.Equal(1 + 3 + 2, diagram.Nodes.Count);
        Assert.Equal(5, diagram.Edges.Count);
        Assert.Equal(0, diagram.Node("start")!.Layer);
        Assert.Equal(1, diagram.Node(StateDiagram.MessageId("approval_request"))!.Layer);
        Assert.Equal(2, diagram.Node(StateDiagram.MessageId("approval_decision"))!.Layer);
        Assert.Equal(3, diagram.Node(StateDiagram.OutcomeId("settled"))!.Layer);
        Assert.Empty(diagram.Unreachable);
        Assert.Equal(4, diagram.LayerCount);
    }

    [Fact]
    public void TheTextNamesWhoActsAndHowOftenARuleMayRepeat()
    {
        string text = StateDiagram.From(ParticipantFixtures.Contract()).ToText();

        Assert.Contains("start -> approval_request [buyer]\n", text, System.StringComparison.Ordinal);
        Assert.Contains("approval_request -> approval_decision [approver]\n", text, System.StringComparison.Ordinal);
        Assert.Contains("approval_request -> expired [buyer, at most 1]\n", text, System.StringComparison.Ordinal);
    }

    [Fact]
    public void AStateNoRuleReachesIsFlagged()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());
        editor.AddMessage("orphan", "procurement.approval", "buyer", "approver");

        StateDiagram diagram = StateDiagram.From(editor.Current);

        Assert.Equal(["orphan"], diagram.Unreachable.Select(node => node.Label));
        Assert.Contains("orphan is not reached", diagram.ToText(), System.StringComparison.Ordinal);
    }

    [Fact]
    public void ARuleEditedInTheTableChangesTheDiagram()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());
        StateDiagram before = StateDiagram.From(editor.Current);

        editor.RemoveTransition(1);
        StateDiagram after = StateDiagram.From(editor.Current);

        Assert.Equal(4, after.Edges.Count);
        Assert.Contains(after.Unreachable, node => node.Label == "approval_decision");
        Assert.Empty(before.Unreachable);
    }

    [Fact]
    public void AnEmptyContractHasOnlyAStart()
    {
        StateDiagram diagram = StateDiagram.From(ProcessContract.Empty("c"));

        Assert.Single(diagram.Nodes);
        Assert.Empty(diagram.Edges);
        Assert.Equal(1, diagram.LayerCount);
    }
}
