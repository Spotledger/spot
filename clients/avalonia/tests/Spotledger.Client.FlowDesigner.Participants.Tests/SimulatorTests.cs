using System;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The partner simulator: it plays the other lanes from the contract, its payloads are held to the
/// contract's field list, and a simulation never reaches a real link.
public sealed class SimulatorTests
{

    private static PartnerSimulator Simulator() => new(ParticipantFixtures.Contract(), ParticipantFixtures.Buyer);

    [Fact]
    public void ThePartnerPlaysExactlyTheMessagesTheOtherLanesSend()
    {
        PartnerSimulator simulator = Simulator();

        Assert.Equal(["approval_decision", "documents_requested"], simulator.Steps.Select(step => step.Message.Name));
        Assert.All(simulator.Steps, step => Assert.Equal("approver", step.FromRole));
        Assert.All(simulator.Steps, step => Assert.True(step.IsCanned));
    }

    [Fact]
    public void TheCannedPayloadIsDeterministicAndFitsTheContract()
    {
        ContractMessage request = ParticipantFixtures.Contract().Message("approval_request")!;

        string payload = MessagePayload.Canned(request);

        Assert.Equal(payload, MessagePayload.Canned(request));
        Assert.Equal("{\"amount_minor\":100,\"currency\":\"sam\",\"memo\":\"sample memo\",\"cost_code\":\"sample cost_code\"}", payload);
        Assert.Empty(MessagePayload.Check(request, payload));
    }

    [Theory]
    [InlineData("{\"decision\":\"approved\"}", true)]
    [InlineData("{\"decision\":\"approved\",\"comment\":\"ok\"}", true)]
    [InlineData("{}", false)]
    [InlineData("[]", false)]
    [InlineData("not json", false)]
    [InlineData("{\"decision\":1}", false)]
    [InlineData("{\"decision\":\"this text is far too long\"}", false)]
    [InlineData("{\"decision\":\"ok\",\"extra\":\"x\"}", false)]
    public void APayloadMustMatchTheFieldListExactly(string json, bool fits)
    {
        ContractMessage decision = ParticipantFixtures.Contract().Message("approval_decision")!;

        Assert.Equal(fits, MessagePayload.Check(decision, json).Count == 0);
    }

    [Theory]
    [InlineData("{\"amount_minor\":12.5,\"currency\":\"EUR\"}")]
    [InlineData("{\"amount_minor\":1e3,\"currency\":\"EUR\"}")]
    [InlineData("{\"amount_minor\":1.0,\"currency\":\"EUR\"}")]
    [InlineData("{\"amount_minor\":\"100\",\"currency\":\"EUR\"}")]
    public void MoneyIsAWholeNumberOfMinorUnitsAndNeverAFloat(string json)
    {
        ContractMessage request = ParticipantFixtures.Contract().Message("approval_request")!;

        Assert.NotEmpty(MessagePayload.Check(request, json));
        Assert.Empty(MessagePayload.Check(request, "{\"amount_minor\":1250,\"currency\":\"EUR\"}"));
    }

    [Fact]
    public void AProblemNeverEchoesTheMemberThePayloadSupplied()
    {
        ContractMessage decision = ParticipantFixtures.Contract().Message("approval_decision")!;

        System.Collections.Generic.IReadOnlyList<string> problems = MessagePayload.Check(decision, "{\"decision\":\"ok\",\"canary_member_0123456789\":1}");

        Assert.Single(problems);
        Assert.DoesNotContain("canary_member", problems[0], StringComparison.Ordinal);
    }

    [Fact]
    public void AnEditedPayloadIsKeptAndABadOneIsRefused()
    {
        PartnerSimulator simulator = Simulator();

        Assert.True(simulator.SetPayload("approval_decision", "{\"decision\":\"rejected\",\"comment\":\"too dear\"}", out _));
        Assert.False(simulator.Step("approval_decision")!.IsCanned);
        Assert.False(simulator.SetPayload("approval_decision", "{\"decision\":\"x\",\"surprise\":1}", out var problems));
        Assert.NotEmpty(problems);
        Assert.Equal("{\"decision\":\"rejected\",\"comment\":\"too dear\"}", simulator.Step("approval_decision")!.PayloadJson);

        Assert.True(simulator.UseCanned("approval_decision"));
        Assert.True(simulator.Step("approval_decision")!.IsCanned);
        Assert.False(simulator.SetPayload("approval_request", "{}", out _));
    }

    [Fact]
    public void ADelayIsWholeSecondsWithinABound()
    {
        PartnerSimulator simulator = Simulator();

        Assert.True(simulator.SetDelay("approval_decision", 90_000));
        Assert.False(simulator.SetDelay("approval_decision", -1));
        Assert.False(simulator.SetDelay("approval_decision", PartnerSimulator.MaxDelaySeconds + 1));
        Assert.False(simulator.SetDelay("nope", 1));
        Assert.Equal(90_000, simulator.Step("approval_decision")!.DelaySeconds);
    }

    [Fact]
    public void AnEditedContractKeepsTheChoicesThatStillFit()
    {
        PartnerSimulator simulator = Simulator();
        simulator.SetPayload("approval_decision", "{\"decision\":\"rejected\"}", out _);
        simulator.SetFailure("documents_requested", SimulatedFailure.Duplicate);
        ContractEditor editor = new(ParticipantFixtures.Contract());
        editor.AddField("approval_decision", "reason", ContractFieldKind.Text, required: true);

        simulator.UseContract(editor.Current);

        Assert.Equal(SimulatedFailure.Duplicate, simulator.Step("documents_requested")!.Failure);
        // The edited payload no longer carries the new required field, so it falls back to canned.
        Assert.True(simulator.Step("approval_decision")!.IsCanned);
        Assert.Contains("reason", simulator.Step("approval_decision")!.PayloadJson, StringComparison.Ordinal);
    }

    [Fact]
    public void TheRequestIsADryRunAndNamesNoLink()
    {
        PartnerSimulator simulator = Simulator();
        simulator.SetFailure("approval_decision", SimulatedFailure.Timeout);
        simulator.SetDelay("approval_decision", 90_000);

        SimulationRequest request = simulator.ToRequest(ParticipantFixtures.BuyerHalf());

        Assert.True(request.DryRun);
        Assert.Equal("buy_approval", request.ProcessName);
        Assert.Equal("buyer", request.OwnRole);
        Assert.Equal(ParticipantFixtures.Contract().ToText(), request.ContractText);
        Assert.Equal(ParticipantFixtures.BuyerHalf().ToText(), request.ProcessText);
        SimulatedMessage timeout = request.Messages.Single(message => message.Message == "approval_decision");
        Assert.Equal(SimulatedFailure.Timeout, timeout.Failure);
        Assert.Equal(90_000, timeout.DelaySeconds);
        string[] members = [.. typeof(SimulationRequest).GetProperties().Select(property => property.Name)];
        Assert.DoesNotContain(members, name => name.Contains("Link", StringComparison.OrdinalIgnoreCase)
            || name.Contains("Peer", StringComparison.OrdinalIgnoreCase)
            || name.Contains("Address", StringComparison.OrdinalIgnoreCase)
            || name.Contains("Tenant", StringComparison.OrdinalIgnoreCase));
    }

    [Fact]
    public void TheHistoryListsEveryMessageInTheServersOrderWithItsDeliveryState()
    {
        SimulationResult result = new(
            "sim_1",
            SimulationOutcome.NeedsAttention,
            [
                new SimulationEvent(3, SimulationDirection.Received, "documents_requested", "approver", DeliveryState.Duplicate, 5, "second copy"),
                new SimulationEvent(1, SimulationDirection.Sent, "approval_request", "approver", DeliveryState.Acknowledged, 0),
                new SimulationEvent(2, SimulationDirection.Received, "documents_requested", "approver", DeliveryState.Delivered, 5),
                new SimulationEvent(4, SimulationDirection.Received, "approval_decision", "approver", DeliveryState.TimedOut, 86_400),
            ],
            OutcomeName: "expired");

        SimulationHistory history = new(result);

        Assert.Equal([1, 2, 3, 4], history.Rows.Select(row => row.Sequence));
        Assert.Equal("Simulation sim_1 needs attention in expired.", history.Heading);
        Assert.Equal("approval_request sent to approver: Acknowledged at 0s", history.Rows[0].Text);
        Assert.Equal("documents_requested received from approver: Duplicate at 5s (second copy)", history.Rows[2].Text);
        Assert.Single(history.InState(DeliveryState.TimedOut));
        Assert.Contains("4. approval_decision received from approver: Timed out at 86400s", history.ToText(), StringComparison.Ordinal);
    }
}
