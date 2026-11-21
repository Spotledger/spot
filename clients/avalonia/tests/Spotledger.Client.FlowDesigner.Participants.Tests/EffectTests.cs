using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The effect badges from the server's effect summary, and the test-run effect preview.
public sealed class EffectTests
{

    [Fact]
    public void ANodeBadgeShowsItsWorstReversibilityAndWhereItsEffectsGo()
    {
        EffectBadges badges = new(ParticipantFixtures.EffectSummary());

        NodeEffectBadge send = badges.Badge("send_message")!;
        Assert.Equal(EffectReversibility.Irreversible, send.Worst);
        Assert.True(send.HasExternal);
        Assert.False(send.HasInternal);
        Assert.Equal("Irreversible, external", send.Text);

        NodeEffectBadge form = badges.Badge("form")!;
        Assert.Equal("Reversible, internal", form.Text);
        Assert.Equal("Compensable, internal", badges.Badge("commit")!.Text);
        Assert.Null(badges.Badge("wait_message"));
        Assert.Equal(3, badges.All.Count);
    }

    [Fact]
    public void AMixedNodeIsWordedInternalAndExternalAndTheWorstEffectWins()
    {
        EffectBadges badges = new(
        [
            new NodeEffectsInfo("n",
            [
                new EffectBadgeInfo("a:s", "c1", EffectPlace.Internal, EffectReversibility.Reversible, "cancel"),
                new EffectBadgeInfo("a:t", "c2", EffectPlace.External, EffectReversibility.Compensable, "refund"),
            ]),
            new NodeEffectsInfo("empty", []),
        ]);

        NodeEffectBadge badge = badges.Badge("n")!;

        Assert.Equal("Compensable, internal and external", badge.Text);
        Assert.Contains("c2 (compensable, external, by refund)", badge.HelpText, System.StringComparison.Ordinal);
        Assert.Null(badges.Badge("empty"));
    }

    [Fact]
    public void ThePreviewGroupsReceiptsByStepInTheOrderOfTheProcessAndFlagsWhatWasNotDeclared()
    {
        EffectPreviewPanel panel = new(ParticipantFixtures.BuyerHalf(), ParticipantFixtures.Preview());

        Assert.Equal(["form", "send_message", "commit"], panel.Groups.Select(group => group.NodeId));
        EffectPreviewGroup commit = panel.Groups[2];
        Assert.True(commit.HasUndeclared);
        Assert.Contains("undeclared: no reversal declared", commit.Lines[0].Text, System.StringComparison.Ordinal);
        Assert.False(panel.Groups[0].HasUndeclared);
        Assert.True(panel.HasUndeclared);
        Assert.Equal(1, panel.UndeclaredCount);
        Assert.Equal("Effects of 3 steps. 1 undeclared.", panel.Heading);
        Assert.Single(panel.Totals);
    }

    [Fact]
    public void AReceiptForAStepTheProcessNoLongerHasIsStillShownLast()
    {
        EffectPreview preview = new(
            [
                new NodePreview("gone", [new PreviewEffect("send", "Old", "a:b", "cls", EffectPlace.External, 1)]),
                new NodePreview("form", [new PreviewEffect("create", "Draft", "a:c", "cls", EffectPlace.Internal, 1)]),
            ],
            [],
            0);

        EffectPreviewPanel panel = new(ParticipantFixtures.BuyerHalf(), preview);

        Assert.Equal(["form", "gone"], panel.Groups.Select(group => group.NodeId));
        Assert.False(panel.HasUndeclared);
    }

    [Fact]
    public void ARunWithNoEffectsSaysSo()
    {
        EffectPreviewPanel panel = new(ParticipantFixtures.BuyerHalf(), new EffectPreview([], [], 0));

        Assert.Empty(panel.Groups);
        Assert.Equal("The test run produced no effects.", panel.Heading);
        Assert.Contains("no effects", panel.ToText(), System.StringComparison.Ordinal);
        Assert.Empty(new EffectPreviewPanel(ParticipantFixtures.BuyerHalf(), null).Groups);
    }

    [Fact]
    public void ThePreviewTextListsEveryReceiptUnderItsStep()
    {
        string text = new EffectPreviewPanel(ParticipantFixtures.BuyerHalf(), ParticipantFixtures.Preview()).ToText();

        Assert.Contains("send_message:\n  partner_message x1 (external): send Approval Request in procurement:send\n", text, System.StringComparison.Ordinal);
        Assert.Contains("Total partner_message (external): 1 effects, 1 records", text, System.StringComparison.Ordinal);
    }
}
