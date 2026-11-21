using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner.Participants;

/// One problem the server reports about a contract, or about a process half checked against one.
/// The code is the contract's (<c>bpm_contract.*</c>); the subject is the role, message, outcome,
/// node or signature it anchors on, or empty for the contract as a whole; the message names no
/// payload value.
public sealed record ContractProblem(string Code, string Subject, string Message);

/// How a named part of a contract changed between two versions.
public enum ContractChangeKind
{
    Added,
    Removed,
    Modified,
}

/// Whether a running conversation can follow a change. The server decides; the client shows it.
public enum ContractCompatibility
{
    Additive,
    Breaking,
}

/// One difference between two contract versions, as the server's <c>diff</c> classifies it.
public sealed record ContractChangeInfo(
    string Subject,
    string Name,
    ContractChangeKind Change,
    ContractCompatibility Compatibility,
    string Detail);

/// One row of the contract's version list.
public sealed record ContractVersionSummary(int Version, string ContentHash, int SignatureCount);

/// What the server says about one signature on a version: it verifies or it does not, and whether
/// it covers this version's content hash. The client never verifies a signature itself.
public sealed record ContractSignatureStatus(string Role, bool Verified, bool CoversThisVersion);

/// The server's answer to a contract save.
public sealed record ContractSaved(int Version, string ContentHash);

/// How reversible one declared effect is, in the server's summary order.
public enum EffectReversibility
{
    Reversible,
    Compensable,
    Irreversible,
}

/// Whether an effect stays inside the operation or leaves the building.
public enum EffectPlace
{
    Internal,
    External,
}

/// One declared effect of a node, from the server's <c>effect_summary</c>. <see cref="By"/> is the
/// lifecycle operation that reverses it, or the compensating action; absent for an irreversible
/// effect.
public sealed record EffectBadgeInfo(
    string Step,
    string Class,
    EffectPlace Place,
    EffectReversibility Reversibility,
    string? By = null);

/// The declared effects of one node.
public sealed record NodeEffectsInfo(string NodeId, IReadOnlyList<EffectBadgeInfo> Effects);

/// One effect a test run produced, with the reason it was not declared when it was not.
public sealed record PreviewEffect(
    string Operation,
    string Doctype,
    string Step,
    string Class,
    EffectPlace Place,
    long Count,
    string? Undeclared = null);

/// What one node produced in a test run.
public sealed record NodePreview(string NodeId, IReadOnlyList<PreviewEffect> Effects);

/// The total of one effect class across the run.
public sealed record EffectClassTotal(string Class, EffectPlace Place, long Effects, long Count);

/// The server's effect preview of a test run: receipts grouped by node, totals by class, and how
/// many effects were not declared.
public sealed record EffectPreview(
    IReadOnlyList<NodePreview> Nodes,
    IReadOnlyList<EffectClassTotal> Totals,
    long Undeclared);

/// Everything the participant designer asks the server beyond the base designer's seam: contract
/// checks, the conformance of the local half, version diffs, signatures, the contract store and the
/// effect summary and preview. The server is authoritative for all of them; a host binds this to the
/// typed client and the tests bind it to a fake. The client sends the canonical text forms and never
/// judges them itself.
public interface IParticipantServer
{
    Task<IReadOnlyList<ContractProblem>> CheckContractAsync(string contractText, CancellationToken cancellationToken);

    Task<IReadOnlyList<ContractProblem>> ConformsAsync(
        string processText,
        string contractText,
        string role,
        CancellationToken cancellationToken);

    Task<IReadOnlyList<ContractChangeInfo>> DiffAsync(
        string oldContractText,
        string newContractText,
        CancellationToken cancellationToken);

    Task<IReadOnlyList<ContractVersionSummary>> ListContractVersionsAsync(string contractId, CancellationToken cancellationToken);

    Task<string?> LoadContractTextAsync(string contractId, int? version, CancellationToken cancellationToken);

    Task<ContractSaved> SaveContractAsync(string contractText, CancellationToken cancellationToken);

    Task<IReadOnlyList<ContractSignatureStatus>> VerifySignaturesAsync(string contractText, CancellationToken cancellationToken);

    Task<IReadOnlyList<NodeEffectsInfo>> EffectSummaryAsync(string processText, CancellationToken cancellationToken);

    Task<EffectPreview> EffectPreviewAsync(string runId, CancellationToken cancellationToken);
}

/// How a simulated message fails, when it does. Each one is a case the contract's order, wait
/// limits and idempotency must survive (ADR-080 sections 5.5 and 5.9).
public enum SimulatedFailure
{
    None,
    Reject,
    Timeout,
    Duplicate,
    OutOfOrder,
}

/// One message the simulated partner plays: who sends it, what it carries, how long it is held
/// back (whole seconds, never a clock reading) and how it fails.
public sealed record SimulatedMessage(
    string Message,
    string FromRole,
    string PayloadJson,
    long DelaySeconds,
    SimulatedFailure Failure);

/// What the client asks the server to simulate. It carries no link, no peer address and no tenant
/// other than the author's: a simulation runs in the server's dry-run mode against a stand-in for
/// the other lanes and never reaches a real partner link. <see cref="DryRun"/> is a constant for
/// the server to check and for a test to assert.
public sealed record SimulationRequest(
    string ProcessName,
    int ProcessVersion,
    string ProcessText,
    string ContractText,
    string OwnRole,
    IReadOnlyList<SimulatedMessage> Messages)
{

    public bool DryRun { get; } = true;
}

/// Which way a message went in a simulation, from the author's side.
public enum SimulationDirection
{
    Sent,
    Received,
}

/// The delivery state of one message in a simulation, in the server's words.
public enum DeliveryState
{
    Queued,
    Delivered,
    Acknowledged,
    Applied,
    Rejected,
    TimedOut,
    Duplicate,
    OutOfOrder,
}

/// One entry of a simulation's execution history: a message sent or received and the state it
/// reached. <see cref="Sequence"/> is the server's order; <see cref="AtSeconds"/> is simulated
/// time, never a wall-clock reading.
public sealed record SimulationEvent(
    int Sequence,
    SimulationDirection Direction,
    string Message,
    string Role,
    DeliveryState State,
    long AtSeconds,
    string? Detail = null);

/// How a simulation ended.
public enum SimulationOutcome
{
    Completed,
    Failed,
    Stalled,
    NeedsAttention,
}

/// The server's answer to a simulation. <see cref="UsedRealLink"/> is a guard: a server that ever
/// answers true is refused by the client and its answer is thrown away.
public sealed record SimulationResult(
    string RunId,
    SimulationOutcome Outcome,
    IReadOnlyList<SimulationEvent> Events,
    bool UsedRealLink = false,
    string? OutcomeName = null);

/// The partner simulator's server seam: one call that plays the other lanes from the contract in
/// the server's dry-run mode.
public interface IPartnerSimulatorServer
{
    Task<SimulationResult> RunAsync(SimulationRequest request, CancellationToken cancellationToken);
}

/// A simulation the server refused; <see cref="Code"/> is the contract's error code.
public sealed class SimulationRefusedException : Exception
{

    public SimulationRefusedException(string code, string message)
        : base(message) => Code = code;

    public SimulationRefusedException()
        : this("bpm.simulation.refused", "The server refused the simulation.") { }

    public SimulationRefusedException(string message)
        : this("bpm.simulation.refused", message) { }

    public SimulationRefusedException(string message, Exception inner)
        : base(message, inner) => Code = "bpm.simulation.refused";

    public string Code { get; } = "bpm.simulation.refused";
}
