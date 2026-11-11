using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner.Participants;

/// Turns the server's contract problems into designer problems, so conformance feedback shows on
/// the canvas and in the problem list exactly like the base designer's own: a problem whose subject
/// is a step of the document points at that step, any other points at the process as a whole.
public static class ConformanceFeedback
{

    public static IReadOnlyList<FlowProblem> ToFlowProblems(FlowDocument document, IEnumerable<ContractProblem> problems)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(problems);
        List<FlowProblem> result = [];
        foreach (ContractProblem problem in problems)
        {
            string? node = problem.Subject.Length > 0 && document.Node(problem.Subject) is not null ? problem.Subject : null;
            result.Add(new FlowProblem(problem.Code, problem.Message, FlowProblemSeverity.Error, node));
        }

        return result;
    }
}

/// One participant-designer session: the document being modelled, the contract it is one half of,
/// the role it plays, and everything the server says about the pair. It never decides what a
/// contract or a conformance problem means (the server does) and never reads the wall clock.
public sealed class ParticipantSession
{

    private readonly IParticipantServer _server;

    private readonly IPartnerSimulatorServer _simulatorServer;

    public ParticipantSession(
        IParticipantServer server,
        IPartnerSimulatorServer simulatorServer,
        FlowDocument document,
        FlowEditor editor,
        ContractEditor contract,
        string ownRole,
        FlowKindCatalogue serverKinds)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(simulatorServer);
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(editor);
        ArgumentNullException.ThrowIfNull(contract);
        ArgumentNullException.ThrowIfNull(ownRole);
        ArgumentNullException.ThrowIfNull(serverKinds);
        _server = server;
        _simulatorServer = simulatorServer;
        Document = document;
        Editor = editor;
        Contract = contract;
        OwnRole = ownRole;
        ServerKinds = serverKinds;
        Simulator = new PartnerSimulator(contract.Current, ownRole);
        contract.Changed += (_, _) =>
        {
            Simulator.UseContract(Contract.Current);
            Changed?.Invoke(this, EventArgs.Empty);
        };
    }

    public FlowDocument Document { get; }

    public FlowEditor Editor { get; }

    public ContractEditor Contract { get; }

    public string OwnRole { get; }

    public FlowKindCatalogue ServerKinds { get; }

    public PartnerSimulator Simulator { get; }

    /// The catalogue for the canvas: the server's kinds plus the participant kinds, with the
    /// contract's messages as the choices.
    public FlowKindCatalogue Catalogue => ParticipantKinds.Extend(ServerKinds, Contract.Current, OwnRole);

    public ParticipantLanes Lanes => new(Contract.Current, OwnRole);

    public IReadOnlyList<ContractProblem> ContractProblems { get; private set; } = [];

    public IReadOnlyList<ContractProblem> ConformanceProblems { get; private set; } = [];

    public IReadOnlyList<ContractChangeInfo> Changes { get; private set; } = [];

    public IReadOnlyList<ContractVersionSummary> Versions { get; private set; } = [];

    public IReadOnlyList<ContractSignatureStatus> SignatureStatuses { get; private set; } = [];

    public EffectBadges Effects { get; private set; } = EffectBadges.Empty;

    public EffectPreviewPanel? Preview { get; private set; }

    public SimulationHistory? Simulation { get; private set; }

    public string? StatusMessage { get; private set; }

    public string? ErrorMessage { get; private set; }

    public bool IsBusy { get; private set; }

    public event EventHandler? Changed;

    /// The generated state diagram of the contract being edited.
    public StateDiagram Diagram => StateDiagram.From(Contract.Current);

    /// The conformance problems as designer problems, for the canvas and the problem list.
    public IReadOnlyList<FlowProblem> ConformanceFlowProblems =>
        ConformanceFeedback.ToFlowProblems(Document, ConformanceProblems);

    /// Everything the problem list shows beyond the base designer's server problems: conformance,
    /// then the designer's own hints about backward edges.
    public IReadOnlyList<FlowProblem> ExtraProblems => [.. ConformanceFlowProblems, .. EdgeModes.Hints(Document)];

    /// The signature rows with the server's verdict where it has given one.
    public IReadOnlyList<SignatureRow> SignatureRows => SignatureRow.Rows(Contract.Current, SignatureStatuses);

    public async Task<FlowActionResult> CheckContractAsync(CancellationToken cancellationToken)
    {
        ContractProblems = await _server.CheckContractAsync(Contract.Export(), cancellationToken).ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return ContractProblems.Count == 0
            ? FlowActionResult.Success(ParticipantStrings.ContractOk)
            : FlowActionResult.Refused(ParticipantStrings.ContractProblems(ContractProblems.Count));
    }

    /// Asks the server whether the local half conforms to the contract, for the role it plays.
    public async Task<FlowActionResult> CheckConformanceAsync(CancellationToken cancellationToken)
    {
        ConformanceProblems = await _server
            .ConformsAsync(Document.ToText(), Contract.Export(), OwnRole, cancellationToken)
            .ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return ConformanceProblems.Count == 0
            ? FlowActionResult.Success(ParticipantStrings.ConformanceOk)
            : FlowActionResult.Refused(ParticipantStrings.ConformanceProblems(ConformanceProblems.Count));
    }

    /// Asks the server what changed between the saved version and this one, and whether each change
    /// is additive or breaking.
    public async Task<IReadOnlyList<ContractChangeInfo>> LoadChangesAsync(CancellationToken cancellationToken)
    {
        Changes = await _server
            .DiffAsync(Contract.Baseline.ToText(), Contract.Current.ToText(), cancellationToken)
            .ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return Changes;
    }

    public async Task<IReadOnlyList<ContractVersionSummary>> LoadVersionsAsync(CancellationToken cancellationToken)
    {
        Versions = await _server.ListContractVersionsAsync(Contract.Current.Id, cancellationToken).ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return Versions;
    }

    public async Task<IReadOnlyList<ContractSignatureStatus>> VerifySignaturesAsync(CancellationToken cancellationToken)
    {
        SignatureStatuses = await _server.VerifySignaturesAsync(Contract.Export(), cancellationToken).ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return SignatureStatuses;
    }

    /// Saves the contract as a new version at the server and makes it the baseline of the diff.
    public async Task<FlowActionResult> SaveContractAsync(CancellationToken cancellationToken)
    {
        if (Contract.ReadOnly)
        {
            return Refuse(ParticipantStrings.ContractReadOnly);
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            ContractSaved saved = await _server.SaveContractAsync(Contract.Export(), cancellationToken).ConfigureAwait(true);
            Contract.MarkSaved(saved.Version);
            StatusMessage = ParticipantStrings.ContractSaved(saved.Version);
            ErrorMessage = null;
            return FlowActionResult.Success(StatusMessage);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    /// Loads the declared effects of every step, for the per-step badges.
    public async Task<EffectBadges> LoadEffectsAsync(CancellationToken cancellationToken)
    {
        IReadOnlyList<NodeEffectsInfo> summary = await _server.EffectSummaryAsync(Document.ToText(), cancellationToken).ConfigureAwait(true);
        Effects = new EffectBadges(summary);
        Changed?.Invoke(this, EventArgs.Empty);
        return Effects;
    }

    /// Loads the effects a test run produced, as receipts grouped by step.
    public async Task<EffectPreviewPanel> LoadPreviewAsync(string runId, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(runId);
        EffectPreview preview = await _server.EffectPreviewAsync(runId, cancellationToken).ConfigureAwait(true);
        Preview = new EffectPreviewPanel(Document, preview);
        Changed?.Invoke(this, EventArgs.Empty);
        return Preview;
    }

    /// Plays the other lanes from the contract against the server's dry-run mode. The plan is
    /// checked first: a payload the contract's field list would not let cross never leaves the
    /// client. An answer that says a real link was used is thrown away and reported, because a
    /// simulation must never reach a partner.
    public async Task<FlowActionResult> RunSimulationAsync(CancellationToken cancellationToken)
    {
        foreach (PartnerStep step in Simulator.Steps)
        {
            if (MessagePayload.Check(step.Message, step.PayloadJson).Count > 0)
            {
                return Refuse(ParticipantStrings.PayloadInvalid(step.Message.Name));
            }
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            SimulationRequest request = Simulator.ToRequest(Document);
            SimulationResult result = await _simulatorServer.RunAsync(request, cancellationToken).ConfigureAwait(true);
            if (result.UsedRealLink)
            {
                Simulation = null;
                return Refuse(ParticipantStrings.SimulationUsedRealLink);
            }

            Simulation = new SimulationHistory(result);
            StatusMessage = Simulation.Heading;
            ErrorMessage = null;
            return FlowActionResult.Success(StatusMessage);
        }
        catch (SimulationRefusedException refused)
        {
            return Refuse(refused.Message);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    private FlowActionResult Refuse(string reason)
    {
        ErrorMessage = reason;
        StatusMessage = null;
        Changed?.Invoke(this, EventArgs.Empty);
        return FlowActionResult.Refused(reason);
    }
}

/// One row of the signature list: who signed, how, and what the server says about it.
public sealed record SignatureRow(string Role, string Algorithm, string KeyId, string State)
{

    public static IReadOnlyList<SignatureRow> Rows(ProcessContract contract, IEnumerable<ContractSignatureStatus> statuses)
    {
        ArgumentNullException.ThrowIfNull(contract);
        ArgumentNullException.ThrowIfNull(statuses);
        List<ContractSignatureStatus> known = [.. statuses];
        return
        [
            .. contract.Signatures.Select(signature =>
            {
                ContractSignatureStatus? status = known.FirstOrDefault(s => string.Equals(s.Role, signature.Role, StringComparison.Ordinal));
                string state = status is null
                    ? ParticipantStrings.SignatureUnchecked
                    : !status.Verified
                        ? ParticipantStrings.SignatureFailed
                        : status.CoversThisVersion ? ParticipantStrings.SignatureVerified : ParticipantStrings.SignatureOtherVersion;
                return new SignatureRow(signature.Role, signature.Algorithm, signature.KeyId, state);
            }),
        ];
    }
}
