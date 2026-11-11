using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The participant session: it carries the server's answers and never judges them.
public sealed class SessionTests
{

    private static ParticipantSession Session(
        out FakeParticipantServer server,
        out FakeSimulatorServer simulator,
        out FlowDocument document)
    {
        server = new FakeParticipantServer();
        simulator = new FakeSimulatorServer();
        document = ParticipantFixtures.BuyerHalf();
        return new ParticipantSession(
            server,
            simulator,
            document,
            new FlowEditor(document),
            new ContractEditor(ParticipantFixtures.Contract(), ParticipantFixtures.SecretCostCode()),
            ParticipantFixtures.Buyer,
            ParticipantFixtures.ServerKinds());
    }

    [Fact]
    public void TheCatalogueHasTheServersKindsAndTheFourParticipantKindsWithTheContractsMessages()
    {
        ParticipantSession session = Session(out _, out _, out _);

        FlowKindCatalogue catalogue = session.Catalogue;

        Assert.NotNull(catalogue.Schema("document"));
        Assert.All(ParticipantKinds.All, kind => Assert.NotNull(catalogue.Schema(kind)));
        FlowKindField send = catalogue.Schema("send_message")!.Fields.Single(f => f.Key == "message");
        Assert.Equal(["approval_request"], send.Options);
        FlowKindField wait = catalogue.Schema("wait_message")!.Fields.Single(f => f.Key == "message");
        Assert.Equal(["approval_decision", "documents_requested"], wait.Options);
        Assert.Equal("purchase_approval", ((FlowStringValue)catalogue.Schema("send_message")!.NewNode("s").ConfigValue("contract")!).Value);
    }

    [Fact]
    public void AKindTheServerDescribesKeepsTheServersSchema()
    {
        FlowKindCatalogue server = new(
        [
            new FlowKindSchema("send_message", "Send to partner", "Messages", [new FlowKindField("message", "Which", FlowFieldType.Text)]),
        ]);

        FlowKindCatalogue merged = ParticipantKinds.Extend(server, ParticipantFixtures.Contract(), "buyer");

        Assert.Equal("Send to partner", merged.DisplayName("send_message"));
        Assert.Equal(FlowFieldType.Text, merged.Schema("send_message")!.Fields.Single().Type);
        Assert.NotNull(merged.Schema("form"));
        Assert.NotNull(merged.Schema("wait_message"));
        Assert.NotNull(merged.Schema("commit"));
    }

    [Fact]
    public async Task ConformanceProblemsBecomeDesignerProblemsOnTheStepsTheyNameAndTheServerIsAskedWithTheCanonicalTexts()
    {
        ParticipantSession session = Session(out FakeParticipantServer server, out _, out FlowDocument document);
        server.ConformanceProblems =
        [
            new ContractProblem("bpm_contract.conformance_timeout", "wait_message", "wait node `wait_message` waits 90000s, longer than the contract's 86400s"),
            new ContractProblem("bpm_contract.conformance_obligation", "approval_request", "the contract obliges role `buyer` to send `approval_request`, but no reachable step does"),
        ];

        FlowActionResult result = await session.CheckConformanceAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        (string process, string contract, string role) = Assert.Single(server.Conformed);
        Assert.Equal(document.ToText(), process);
        Assert.Equal(session.Contract.Export(), contract);
        Assert.Equal("buyer", role);
        IReadOnlyList<FlowProblem> problems = session.ConformanceFlowProblems;
        Assert.Equal("wait_message", problems[0].NodeId);
        Assert.Null(problems[1].NodeId);
        Assert.All(problems, problem => Assert.Equal(FlowProblemSeverity.Error, problem.Severity));
        Assert.Equal(2, session.ExtraProblems.Count(problem => problem.Severity == FlowProblemSeverity.Error));
    }

    [Fact]
    public async Task AConformingHalfSaysSo()
    {
        ParticipantSession session = Session(out _, out _, out _);

        FlowActionResult result = await session.CheckConformanceAsync(TestContext.Current.CancellationToken);

        Assert.True(result.Ok);
        Assert.Equal("This half follows the contract.", result.Message);
        Assert.Empty(session.ConformanceFlowProblems);
    }

    [Fact]
    public async Task TheDiffIsTheServersAndCarriesItsClassification()
    {
        ParticipantSession session = Session(out FakeParticipantServer server, out _, out _);
        server.Changes =
        [
            new ContractChangeInfo("field", "approval_request.memo", ContractChangeKind.Added, ContractCompatibility.Additive, "optional field added"),
            new ContractChangeInfo("timeout", "approval_decision", ContractChangeKind.Modified, ContractCompatibility.Breaking, "wait limit shortened"),
        ];
        session.Contract.NewVersion();

        IReadOnlyList<ContractChangeInfo> changes = await session.LoadChangesAsync(TestContext.Current.CancellationToken);

        (string old, string next) = Assert.Single(server.Diffed);
        Assert.Equal(ParticipantFixtures.Contract().ToText(), old);
        Assert.Equal(session.Contract.Export(), next);
        Assert.Equal(ContractCompatibility.Breaking, changes[1].Compatibility);
        Assert.Equal("Added field approval_request.memo: additive. optional field added", ParticipantStrings.ChangeLine(changes[0]));
        Assert.Equal("Changed timeout approval_decision: breaking. wait limit shortened", ParticipantStrings.ChangeLine(changes[1]));
    }

    [Fact]
    public async Task SavingTheContractMovesTheBaselineToTheServersVersion()
    {
        ParticipantSession session = Session(out FakeParticipantServer server, out _, out _);
        session.Contract.NewVersion();
        server.NextVersion = 2;

        FlowActionResult result = await session.SaveContractAsync(TestContext.Current.CancellationToken);

        Assert.True(result.Ok);
        Assert.Equal("Contract saved as version 2.", result.Message);
        Assert.Equal(2, session.Contract.Baseline.Version);
        Assert.False(session.Contract.IsModified);
        Assert.Equal(session.Contract.Export(), Assert.Single(server.SavedContracts));
    }

    [Fact]
    public async Task AReadOnlyContractIsNeverSent()
    {
        ParticipantSession session = Session(out FakeParticipantServer server, out _, out _);
        session.Contract.ReadOnly = true;

        FlowActionResult result = await session.SaveContractAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.Empty(server.SavedContracts);
    }

    [Fact]
    public async Task ContractProblemsAndSignaturesAreTheServersWords()
    {
        ParticipantSession session = Session(out FakeParticipantServer server, out _, out _);
        server.ContractProblems = [new ContractProblem("bpm_contract.dead_end", "documents_requested", "from this message no path reaches an outcome")];
        server.Statuses = [new ContractSignatureStatus("buyer", true, false)];

        FlowActionResult check = await session.CheckContractAsync(TestContext.Current.CancellationToken);
        await session.VerifySignaturesAsync(TestContext.Current.CancellationToken);

        Assert.False(check.Ok);
        Assert.Equal("The server found 1 problem with the contract.", check.Error);
        SignatureRow row = Assert.Single(session.SignatureRows);
        Assert.Equal(("buyer", "ed25519", "key-buyer-1"), (row.Role, row.Algorithm, row.KeyId));
        Assert.Equal("Signed another version", row.State);
    }

    [Fact]
    public void ASignatureNobodyHasCheckedIsSaidToBeUnchecked()
    {
        ParticipantSession session = Session(out _, out _, out _);

        Assert.Equal("Not checked yet", Assert.Single(session.SignatureRows).State);
    }

    [Fact]
    public async Task TheEffectSummaryAndPreviewComeFromTheServer()
    {
        ParticipantSession session = Session(out FakeParticipantServer server, out _, out _);
        server.Effects = ParticipantFixtures.EffectSummary();
        server.Preview = ParticipantFixtures.Preview();

        EffectBadges badges = await session.LoadEffectsAsync(TestContext.Current.CancellationToken);
        EffectPreviewPanel panel = await session.LoadPreviewAsync("run_9", TestContext.Current.CancellationToken);

        Assert.Equal("Irreversible, external", badges.Badge("send_message")!.Text);
        Assert.True(panel.HasUndeclared);
        Assert.Same(panel, session.Preview);
    }

    [Fact]
    public async Task TheSimulationScenarioWithADuplicateAndATimeoutRunsAgainstTheDryRunMode()
    {
        ParticipantSession session = Session(out _, out FakeSimulatorServer simulator, out _);
        session.Simulator.SetFailure("documents_requested", SimulatedFailure.Duplicate);
        session.Simulator.SetFailure("approval_decision", SimulatedFailure.Timeout);
        session.Simulator.SetDelay("approval_decision", 90_000);
        simulator.Result = new SimulationResult(
            "sim_7",
            SimulationOutcome.NeedsAttention,
            [
                new SimulationEvent(1, SimulationDirection.Sent, "approval_request", "approver", DeliveryState.Acknowledged, 0),
                new SimulationEvent(2, SimulationDirection.Received, "documents_requested", "approver", DeliveryState.Delivered, 1),
                new SimulationEvent(3, SimulationDirection.Received, "documents_requested", "approver", DeliveryState.Duplicate, 1, "same message id"),
                new SimulationEvent(4, SimulationDirection.Received, "approval_decision", "approver", DeliveryState.TimedOut, 86_400),
            ],
            OutcomeName: "expired");

        FlowActionResult result = await session.RunSimulationAsync(TestContext.Current.CancellationToken);

        Assert.True(result.Ok);
        SimulationRequest sent = Assert.Single(simulator.Requests);
        Assert.True(sent.DryRun);
        Assert.Equal(
            [SimulatedFailure.Timeout, SimulatedFailure.Duplicate],
            sent.Messages.Select(message => message.Failure));
        SimulationHistory history = session.Simulation!;
        Assert.Single(history.InState(DeliveryState.Duplicate));
        Assert.Single(history.InState(DeliveryState.TimedOut));
        Assert.Equal("Simulation sim_7 needs attention in expired.", session.StatusMessage);
    }

    [Fact]
    public async Task AnAnswerThatUsedARealLinkIsThrownAwayAndReported()
    {
        ParticipantSession session = Session(out _, out FakeSimulatorServer simulator, out _);
        simulator.Result = new SimulationResult("sim_8", SimulationOutcome.Completed, [], UsedRealLink: true);

        FlowActionResult result = await session.RunSimulationAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.Null(session.Simulation);
        Assert.Contains("never reaches a partner", result.Error, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ARefusalFromTheServerIsShownAndLeavesNoHistory()
    {
        ParticipantSession session = Session(out _, out FakeSimulatorServer simulator, out _);
        simulator.Failure = new SimulationRefusedException("The contract has problems.");

        FlowActionResult result = await session.RunSimulationAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.Equal("The contract has problems.", result.Error);
        Assert.Null(session.Simulation);
    }

    [Fact]
    public async Task AStalePayloadIsReplacedByACannedOneThatFitsTheEditedContract()
    {
        ParticipantSession session = Session(out _, out FakeSimulatorServer simulator, out _);
        // The contract changes under the plan: a new required field the edited payload lacks.
        session.Simulator.SetPayload("approval_decision", "{\"decision\":\"ok\"}", out _);
        session.Contract.AddField("approval_decision", "reason", ContractFieldKind.Text, required: true);
        session.Simulator.SetPayload("approval_decision", "{\"decision\":\"ok\"}", out _);

        FlowActionResult result = await session.RunSimulationAsync(TestContext.Current.CancellationToken);

        // UseContract replaced the stale payload with a canned one that fits, so it does run.
        Assert.True(result.Ok);
        Assert.Contains("reason", simulator.Requests[0].Messages.Single(m => m.Message == "approval_decision").PayloadJson, StringComparison.Ordinal);
    }

    [Fact]
    public void NoStringReachesAPersonWithTheWordCase()
    {
        foreach (string text in ParticipantStrings.Sample())
        {
            Assert.False(System.Text.RegularExpressions.Regex.IsMatch(text, @"\bCases?\b"), text);
        }
    }
}
