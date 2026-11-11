using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The two-organisation purchase approval the tests share: a buyer who asks and an approver who
/// answers, the contract between them, the server's base kinds and fakes whose answers each test
/// scripts. The fakes record every question, so a test can prove what the client sent as well as
/// what it showed. Time is always an argument: nothing here reads a clock.
public static class ParticipantFixtures
{

    public const string Buyer = "buyer";

    public const string Approver = "approver";

    /// The kinds the server offers on its own: the participant kinds are added by the designer.
    public static FlowKindCatalogue ServerKinds() => new(
    [
        new FlowKindSchema("document", "Document", "Work", [
            new FlowKindField("doctype", "Document type", FlowFieldType.Text, Required: true),
        ]),
    ]);

    /// The contract between the two organisations.
    public static ProcessContract Contract() => new(
        "purchase_approval",
        1,
        "Purchase approval",
        [new ContractParticipant(Buyer, ParticipantKind.Tenant), new ContractParticipant(Approver, ParticipantKind.Tenant)],
        [
            new ContractMessage("approval_request", "procurement.approval", 1, Buyer, Approver,
            [
                new ContractField("amount_minor", ContractFieldKind.Whole),
                new ContractField("currency", ContractFieldKind.Text, true, 3),
                new ContractField("memo", ContractFieldKind.Text, false, 200),
                new ContractField("cost_code", ContractFieldKind.Text, false, 20),
            ]),
            new ContractMessage("approval_decision", "procurement.approval", 1, Approver, Buyer,
            [
                new ContractField("decision", ContractFieldKind.Text, true, 16),
                new ContractField("comment", ContractFieldKind.Text, false, 200),
            ]),
            new ContractMessage("documents_requested", "procurement.approval", 1, Approver, Buyer,
            [
                new ContractField("note", ContractFieldKind.Text, true, 200),
            ]),
        ],
        [
            new ContractTransition(Buyer, null, "approval_request", null),
            new ContractTransition(Approver, "approval_request", "approval_decision", null),
            new ContractTransition(Approver, "approval_request", "documents_requested", null),
            new ContractTransition(Buyer, "approval_decision", null, "settled"),
            new ContractTransition(Buyer, "approval_request", null, "expired", 1),
        ],
        [new ContractOutcome("settled", ContractOutcomeKind.Completed), new ContractOutcome("expired", ContractOutcomeKind.Failed)],
        [new ContractTimeout("approval_decision", Buyer, 86_400, "expired")],
        [new ContractSignature(Buyer, "ed25519", "key-buyer-1", new string('a', 64), "c2ln")]);

    /// The classifier of a host whose metadata says the cost code is a secret.
    public static IContractFieldClassifier SecretCostCode() =>
        new TableFieldClassifier(new Dictionary<string, ContractFieldClass> { ["cost_code"] = ContractFieldClass.Secret });

    /// A fixed instant is never needed; a test that wants one uses this offset arithmetic.
    public static DateTimeOffset T0 => new(2026, 10, 27, 9, 0, 0, TimeSpan.Zero);

    /// The process half of the buyer as the server's text form: form, send, wait with a timeout
    /// edge, commit point.
    public static FlowDocument BuyerHalf() => FlowDocument.FromText(
        """
        name = "buy_approval"
        app = "procurement"
        version = 1
        status = "draft"

        [[nodes]]
        id = "form"
        kind = "form"
        assignee = "role:buyer"
        draft_mode = "collect"
        fields = ["amount_minor", "currency"]

        [[nodes]]
        id = "send_message"
        kind = "send_message"
        contract = "purchase_approval"
        message = "approval_request"

        [[nodes]]
        id = "wait_message"
        kind = "wait_message"
        contract = "purchase_approval"
        message = "approval_decision"
        on_timeout = "timeout_edge"
        timeout_secs = 86400

        [[nodes]]
        id = "commit"
        kind = "commit"

        [[nodes]]
        id = "escalate"
        kind = "form"
        assignee = "role:buyer"
        draft_mode = "collect"
        fields = ["reason"]

        [[edges]]
        from = "form"
        to = "send_message"

        [[edges]]
        from = "send_message"
        to = "wait_message"

        [[edges]]
        from = "wait_message"
        to = "commit"

        [[edges]]
        from = "wait_message"
        to = "escalate"
        label = "timeout"
        """);

    public static IReadOnlyList<NodeEffectsInfo> EffectSummary() =>
    [
        new NodeEffectsInfo("form", [new EffectBadgeInfo("procurement:collect", "draft_record", EffectPlace.Internal, EffectReversibility.Reversible, "cancel")]),
        new NodeEffectsInfo("send_message", [new EffectBadgeInfo("procurement:send", "partner_message", EffectPlace.External, EffectReversibility.Irreversible)]),
        new NodeEffectsInfo("commit", [new EffectBadgeInfo("procurement:post", "ledger_entry", EffectPlace.Internal, EffectReversibility.Compensable, "reverse_entry")]),
    ];

    public static EffectPreview Preview() => new(
        [
            new NodePreview("send_message", [new PreviewEffect("send", "Approval Request", "procurement:send", "partner_message", EffectPlace.External, 1)]),
            new NodePreview("form", [new PreviewEffect("create", "Purchase Draft", "procurement:collect", "draft_record", EffectPlace.Internal, 1)]),
            new NodePreview("commit", [new PreviewEffect("post", "Ledger Entry", "procurement:post", "ledger_entry", EffectPlace.Internal, 2, "no reversal declared")]),
        ],
        [new EffectClassTotal("partner_message", EffectPlace.External, 1, 1)],
        1);
}

/// A fake participant server: every answer is scripted, every question recorded.
public sealed class FakeParticipantServer : IParticipantServer
{

    public List<string> CheckedContracts { get; } = [];

    public List<(string Process, string Contract, string Role)> Conformed { get; } = [];

    public List<(string Old, string New)> Diffed { get; } = [];

    public List<string> SavedContracts { get; } = [];

    public IReadOnlyList<ContractProblem> ContractProblems { get; set; } = [];

    public IReadOnlyList<ContractProblem> ConformanceProblems { get; set; } = [];

    public IReadOnlyList<ContractChangeInfo> Changes { get; set; } = [];

    public IReadOnlyList<ContractVersionSummary> Versions { get; set; } = [];

    public IReadOnlyList<ContractSignatureStatus> Statuses { get; set; } = [];

    public IReadOnlyList<NodeEffectsInfo> Effects { get; set; } = [];

    public EffectPreview Preview { get; set; } = new([], [], 0);

    public int NextVersion { get; set; } = 2;

    public Task<IReadOnlyList<ContractProblem>> CheckContractAsync(string contractText, CancellationToken cancellationToken)
    {
        CheckedContracts.Add(contractText);
        return Task.FromResult(ContractProblems);
    }

    public Task<IReadOnlyList<ContractProblem>> ConformsAsync(string processText, string contractText, string role, CancellationToken cancellationToken)
    {
        Conformed.Add((processText, contractText, role));
        return Task.FromResult(ConformanceProblems);
    }

    public Task<IReadOnlyList<ContractChangeInfo>> DiffAsync(string oldContractText, string newContractText, CancellationToken cancellationToken)
    {
        Diffed.Add((oldContractText, newContractText));
        return Task.FromResult(Changes);
    }

    public Task<IReadOnlyList<ContractVersionSummary>> ListContractVersionsAsync(string contractId, CancellationToken cancellationToken) =>
        Task.FromResult(Versions);

    public Task<string?> LoadContractTextAsync(string contractId, int? version, CancellationToken cancellationToken) =>
        Task.FromResult<string?>(null);

    public Task<ContractSaved> SaveContractAsync(string contractText, CancellationToken cancellationToken)
    {
        SavedContracts.Add(contractText);
        return Task.FromResult(new ContractSaved(NextVersion, "hash-" + NextVersion));
    }

    public Task<IReadOnlyList<ContractSignatureStatus>> VerifySignaturesAsync(string contractText, CancellationToken cancellationToken) =>
        Task.FromResult(Statuses);

    public Task<IReadOnlyList<NodeEffectsInfo>> EffectSummaryAsync(string processText, CancellationToken cancellationToken) =>
        Task.FromResult(Effects);

    public Task<EffectPreview> EffectPreviewAsync(string runId, CancellationToken cancellationToken) =>
        Task.FromResult(Preview);
}

/// A fake simulator server: it answers with the scripted result and records the request.
public sealed class FakeSimulatorServer : IPartnerSimulatorServer
{

    public List<SimulationRequest> Requests { get; } = [];

    public SimulationResult? Result { get; set; }

    public Exception? Failure { get; set; }

    public Task<SimulationResult> RunAsync(SimulationRequest request, CancellationToken cancellationToken)
    {
        Requests.Add(request);
        if (Failure is { } failure)
        {
            throw failure;
        }

        return Task.FromResult(Result ?? new SimulationResult("sim_0", SimulationOutcome.Completed, []));
    }
}

/// A fake base-designer server: it answers validation with the scripted problems.
public sealed class FakeDesignerServer : IFlowDesignerServer
{

    public IReadOnlyList<FlowProblem> Problems { get; set; } = [];

    public Task<FlowKindCatalogue> LoadKindsAsync(CancellationToken cancellationToken) =>
        Task.FromResult(ParticipantFixtures.ServerKinds());

    public Task<FlowValidationReport> ValidateAsync(string textForm, CancellationToken cancellationToken) =>
        Task.FromResult(new FlowValidationReport(Problems, 1));

    public Task<FlowDraftSaved> SaveDraftAsync(string textForm, string? expectedContentHash, CancellationToken cancellationToken) =>
        Task.FromResult(new FlowDraftSaved(2, "hash-2"));

    public Task<FlowPublishAccepted> RequestPublishAsync(string textForm, CancellationToken cancellationToken) =>
        Task.FromResult(new FlowPublishAccepted("review-1"));

    public Task<IReadOnlyList<FlowVersionSummary>> ListVersionsAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<FlowVersionSummary>>([]);
}
