using System;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The contract text form: the same canonical bytes the server writes, a faithful round trip, and
/// errors that never echo the document.
public sealed class ContractTests
{

    [Fact]
    public void TheContractRoundTripsThroughItsCanonicalText()
    {
        ProcessContract contract = ParticipantFixtures.Contract();

        string text = contract.ToText();
        ProcessContract again = ProcessContract.FromText(text);

        Assert.Equal(text, again.ToText());
        Assert.Equal(3, again.Messages.Count);
        Assert.Equal(["amount_minor", "currency", "memo", "cost_code"], again.Messages[0].Fields.Select(f => f.Name));
        Assert.Equal(3UL, again.Messages[0].Field("currency")!.MaxLength);
        Assert.False(again.Messages[0].Field("memo")!.Required);
        Assert.Equal("expired", again.Order[4].NextOutcome);
        Assert.Equal(1, again.Order[4].Bound);
        Assert.Equal(86_400, again.Timeouts[0].Seconds);
        Assert.Equal("expired", again.Timeouts[0].Outcome);
        Assert.Equal("key-buyer-1", again.Signatures[0].KeyId);
    }

    [Fact]
    public void TheTextUsesTheServersKeyOrderAndOneScalarPerLine()
    {
        string text = ParticipantFixtures.Contract().ToText();

        Assert.StartsWith("# Spotledger process contract (spot-bpm-contract).", text, StringComparison.Ordinal);
        Assert.Contains("id = \"purchase_approval\"\nversion = 1\ntitle = \"Purchase approval\"\n\n[[participants]]\nrole = \"buyer\"\nkind = \"tenant\"\n", text, StringComparison.Ordinal);
        Assert.Contains("[[messages.fields]]\nname = \"currency\"\nkind = \"string\"\nrequired = true\nmax_len = 3\n", text, StringComparison.Ordinal);
        Assert.Contains("[[order]]\nby = \"buyer\"\nnext = \"approval_request\"\n", text, StringComparison.Ordinal);
        Assert.Contains("[[order]]\nby = \"buyer\"\nafter = \"approval_request\"\noutcome = \"expired\"\nbound = 1\n", text, StringComparison.Ordinal);
        Assert.EndsWith("signature = \"c2ln\"\n", text, StringComparison.Ordinal);
    }

    [Fact]
    public void ASectionThisBuildDoesNotKnowIsIgnored()
    {
        string text = ParticipantFixtures.Contract().ToText() + "\n[[carrier_hints]]\nprefer = \"webhook\"\n";

        ProcessContract read = ProcessContract.FromText(text);

        Assert.Equal(ParticipantFixtures.Contract().ToText(), read.ToText());
    }

    [Theory]
    [InlineData("id = \"x\"\nversion = 1\n[[order]]\nby = \"a\"\nnext = \"m\"\noutcome = \"o\"\n")]
    [InlineData("id = \"x\"\nversion = 1\n[[order]]\nby = \"a\"\n")]
    [InlineData("id = \"x\"\nversion = 1\nsurprise = 3\n")]
    [InlineData("id = \"x\"\nversion = 1\n[[participants]]\nrole = \"a\"\nkind = \"alien\"\n")]
    [InlineData("id = \"x\"\nversion = 0\n")]
    [InlineData("version = 1\n")]
    [InlineData("id = \"x\"\nversion = 1\n[[messages.fields]]\nname = \"f\"\nkind = \"string\"\nrequired = true\n")]
    [InlineData("id = \"x\"\nversion = 1\nversion = 2\n")]
    [InlineData("[single]\nid = \"x\"\n")]
    public void AContractThatIsNotShapedRightIsRefused(string text)
    {
        Assert.Throws<FlowTextException>(() => ProcessContract.FromText(text));
    }

    [Fact]
    public void AnErrorNeverEchoesTheDocument()
    {
        const string Canary = "canary-secret-0123456789";
        string text = "id = \"x\"\nversion = 1\n" + Canary + " = 3\n";

        FlowTextException error = Assert.Throws<FlowTextException>(() => ProcessContract.FromText(text));

        Assert.DoesNotContain(Canary, error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain(Canary, error.Problem, StringComparison.Ordinal);
    }

    [Fact]
    public void ATextOverTheSizeLimitIsRefusedBeforeItIsParsed()
    {
        string text = "id = \"x\"\nversion = 1\n# " + new string('x', ContractLimits.MaxTextBytes) + "\n";

        Assert.Throws<FlowTextException>(() => ProcessContract.FromText(text));
    }

    [Fact]
    public void TooManyTablesOfOneKindAreRefused()
    {
        string text = "id = \"x\"\nversion = 1\n" + string.Concat(Enumerable.Repeat("[[participants]]\nrole = \"r\"\nkind = \"tenant\"\n", ContractLimits.MaxRoles + 1));

        Assert.Throws<FlowTextException>(() => ProcessContract.FromText(text));
    }

    [Fact]
    public void TheOtherLanesAreEveryRoleButTheOwnOne()
    {
        ProcessContract contract = ParticipantFixtures.Contract();

        Assert.Equal([ParticipantFixtures.Approver], contract.OtherParticipants(ParticipantFixtures.Buyer).Select(p => p.Role));
        Assert.Equal(["approval_request"], contract.SentBy(ParticipantFixtures.Buyer).Select(m => m.Name));
        Assert.Equal(["approval_decision", "documents_requested"], contract.ReceivedBy(ParticipantFixtures.Buyer).Select(m => m.Name));
    }
}
