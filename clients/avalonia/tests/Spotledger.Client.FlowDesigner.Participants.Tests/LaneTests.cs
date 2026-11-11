using System.Linq;
using Avalonia;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The participant lanes: the other organisations as labelled bands with hidden insides, the own
/// lane highlighted, and the contract's messages as the only lines that reach another lane.
public sealed class LaneTests
{

    [Fact]
    public void TheOwnRoleIsHighlightedAndEveryOtherRoleIsALane()
    {
        ParticipantLanes lanes = new(ParticipantFixtures.Contract(), ParticipantFixtures.Buyer);

        Assert.True(lanes.Own.IsOwn);
        Assert.Equal("buyer", lanes.Own.Role);
        LaneInfo other = Assert.Single(lanes.Others);
        Assert.Equal("approver", other.Role);
        Assert.False(other.IsOwn);
        Assert.StartsWith("You: buyer", ParticipantStrings.LaneLabel(lanes.Own), System.StringComparison.Ordinal);
        Assert.Equal("approver (organisation)", ParticipantStrings.LaneLabel(other));
    }

    [Fact]
    public void AnExternalSystemIsLabelledAsOne()
    {
        ContractEditor editor = new(ProcessContract.Empty("c"));
        editor.AddParticipant("us", ParticipantKind.Tenant);
        editor.AddParticipant("bank", ParticipantKind.External);
        ParticipantLanes lanes = new(editor.Current, "us");

        Assert.Equal("bank (external system)", ParticipantStrings.LaneLabel(lanes.Others[0]));
    }

    [Fact]
    public void MessagesAreTheOnlyLinksAndEachNamesTheLaneOnTheFarEnd()
    {
        ParticipantLanes lanes = new(ParticipantFixtures.Contract(), ParticipantFixtures.Buyer);

        System.Collections.Generic.IReadOnlyList<LaneMessageLink> links = lanes.Links(ParticipantFixtures.BuyerHalf());

        Assert.Equal(2, links.Count);
        Assert.Equal(new LaneMessageLink("send_message", "approval_request", "approver", MessageDirection.Out, true), links[0]);
        Assert.Equal(new LaneMessageLink("wait_message", "approval_decision", "approver", MessageDirection.In, true), links[1]);
        Assert.Equal(2, lanes.LinksOf(ParticipantFixtures.BuyerHalf(), "approver").Count);
        Assert.Empty(lanes.LinksOf(ParticipantFixtures.BuyerHalf(), "buyer"));
    }

    [Fact]
    public void AFlowEdgeJoinsTwoStepsOfTheOwnLaneSoNoFlowEdgeCrossesLanes()
    {
        FlowDocument document = ParticipantFixtures.BuyerHalf();
        ParticipantLanes lanes = new(ParticipantFixtures.Contract(), ParticipantFixtures.Buyer);
        System.Collections.Generic.HashSet<string> steps = [.. document.Nodes.Select(node => node.Id)];

        Assert.All(document.Edges, edge =>
        {
            Assert.Contains(edge.From, steps);
            Assert.Contains(edge.To, steps);
        });
        Assert.All(lanes.Links(document), link => Assert.NotEqual(lanes.OwnRole, link.Role));
    }

    [Fact]
    public void AMessageTheContractDoesNotDefineOrDefinesTheOtherWayIsMarkedUnknown()
    {
        FlowDocument document = ParticipantFixtures.BuyerHalf();
        FlowEditor editor = new(document);
        editor.Execute(new SetConfigCommand("send_message", "message", FlowValue.Text("approval_decision")));
        editor.Execute(new SetConfigCommand("wait_message", "message", FlowValue.Text("ghost")));
        ParticipantLanes lanes = new(ParticipantFixtures.Contract(), ParticipantFixtures.Buyer);

        var links = lanes.Links(document);

        Assert.False(links[0].Known);
        Assert.False(links[1].Known);
        Assert.Equal(string.Empty, links[1].Role);
    }

    [Fact]
    public void TheBandsWrapTheStepsAndStackTheOtherLanesAlternately()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());
        editor.AddParticipant("bank", ParticipantKind.External);
        editor.AddParticipant("auditor", ParticipantKind.External);
        ParticipantLanes lanes = new(editor.Current, "buyer");
        Rect steps = new(100, 100, 600, 80);

        var bands = lanes.Bands(steps);

        Assert.Equal(4, bands.Count);
        LaneBand own = bands[0];
        Assert.True(own.IsOwn);
        Assert.True(own.Bounds.Contains(steps));
        Assert.True(bands[1].Above);
        Assert.False(bands[2].Above);
        Assert.True(bands[3].Above);
        Assert.True(bands[1].Bounds.Bottom < own.Bounds.Top);
        Assert.True(bands[2].Bounds.Top > own.Bounds.Bottom);
        Assert.True(bands[3].Bounds.Bottom < bands[1].Bounds.Top);
        Assert.All(bands, band => Assert.Equal(own.Bounds.Width, band.Bounds.Width));
    }

    [Fact]
    public void AMessagePortSitsOnTheNearEdgeOfItsBandAboveTheStep()
    {
        ParticipantLanes lanes = new(ParticipantFixtures.Contract(), "buyer");
        Rect step = new(300, 100, 160, 56);
        LaneBand above = lanes.Bands(step)[1];

        Point port = ParticipantLanes.Port(above, step);

        Assert.Equal(above.Bounds.Bottom, port.Y);
        Assert.Equal(step.Center.X, port.X);
    }

    [Fact]
    public void WithNoContractThereAreNoOtherLanes()
    {
        ParticipantLanes lanes = new(null, "buyer");

        Assert.Empty(lanes.Others);
        Assert.All(lanes.Links(ParticipantFixtures.BuyerHalf()), link => Assert.False(link.Known));
        Assert.Single(lanes.Bands(new Rect(0, 0, 100, 50)));
    }
}
