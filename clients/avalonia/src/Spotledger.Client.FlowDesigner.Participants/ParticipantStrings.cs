using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;

namespace Spotledger.Client.FlowDesigner.Participants;

/// Every user-visible string of this project, in one place (ADR-073). None of them names a run:
/// the project talks about steps, messages, lanes and contracts, so the organisation's run label
/// (ADR-080) never has to be spliced in here. <see cref="Sample"/> lists one rendering of each for
/// the scan test that fails when the literal word "Case" reaches a person.
public static class ParticipantStrings
{

    // Refusals of a contract edit.
    public static string ContractReadOnly => "This contract is read-only for your role.";

    public static string TooLarge => "That is more than a contract can hold.";

    public static string BadName => "A name starts with a lower-case letter and then has lower-case letters, digits and underscores.";

    public static string BadTopic => "A topic has at least two dotted lower-case parts, such as procurement.approval.";

    public static string BadFieldName => "A field name is one or more dotted lower-case parts.";

    public static string BadNumber => "That number is too small.";

    public static string AlreadyExists => "That already exists in the contract.";

    public static string NotFound => "That is not in the contract.";

    public static string RoleInUse => "That role still sends, receives or waits for a message. Remove those first.";

    public static string UnknownRole => "Both ends of a message must be roles of the contract.";

    public static string SelfMessage => "A message goes from one role to another, not to the same role.";

    public static string MaxLengthOnlyText => "A longest length applies to text fields only.";

    public static string OneNext => "An order rule leads to either the next message or an outcome, not both.";

    public static string UnknownReference => "That names a role, message or outcome the contract does not have.";

    public static string ContractFileUnreadable(string reason) => "That file is not a contract this version can read (" + reason + ").";

    // The mapping editor.
    public static string FieldNotInContract => "Only the fields the contract lists for this message can be mapped.";

    public static string MappingOneLine => "A mapping expression is one line.";

    public static string SecretFieldMapped(string field) =>
        "The field " + field + " is classified as secret and can never cross, so it cannot be mapped.";

    public static string RequiredFieldUnmapped(string field) => "The required field " + field + " has no expression.";

    public static string BadExpression(string field, string message) => "The expression for " + field + " is not valid: " + message;

    // Checks and saves.
    public static string ContractOk => "The server found no problems with the contract.";

    public static string ContractProblems(int count) =>
        count == 1 ? "The server found 1 problem with the contract." : "The server found " + Number(count) + " problems with the contract.";

    public static string ConformanceOk => "This half follows the contract.";

    public static string ConformanceProblems(int count) =>
        count == 1 ? "1 place where this half departs from the contract." : Number(count) + " places where this half departs from the contract.";

    public static string ContractSaved(int version) => "Contract saved as version " + Number(version) + ".";

    // Signatures.
    public static string SignatureUnchecked => "Not checked yet";

    public static string SignatureVerified => "Verified for this version";

    public static string SignatureFailed => "Does not verify";

    public static string SignatureOtherVersion => "Signed another version";

    // Lanes and edges.
    public static string LaneLabel(LaneInfo lane)
    {
        ArgumentNullException.ThrowIfNull(lane);
        string kind = lane.Kind == ParticipantKind.External ? "external system" : "organisation";
        return lane.IsOwn ? "You: " + lane.Role + " (" + kind + ")" : lane.Role + " (" + kind + ")";
    }

    public static string MessageLinkLabel(LaneMessageLink link)
    {
        ArgumentNullException.ThrowIfNull(link);
        return link.Direction == MessageDirection.Out ? link.Message + " →" : "→ " + link.Message;
    }

    public static string BackEdgeUntyped(string from, string to) =>
        "The edge from " + from + " back to " + to + " goes backwards. Mark it Undo or Rewind.";

    public static string ModeOnForwardEdge(string from, string to, EdgeMode mode) =>
        "The edge from " + from + " to " + to + " is marked " + mode.Word() + " but does not go backwards.";

    public static string EdgeModeWord(EdgeMode mode) => mode.Word();

    public static string CommitRegionLabel(string commit) => "Held until " + commit;

    // Effects.
    public static string BadgeText(EffectReversibility worst, bool internalEffect, bool external)
    {
        string place = internalEffect && external ? "internal and external" : external ? "external" : "internal";
        return Word(worst) + ", " + place;
    }

    public static string BadgeHelp(string nodeId, IReadOnlyList<EffectBadgeInfo> effects)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        ArgumentNullException.ThrowIfNull(effects);
        return nodeId + ": " + string.Join("; ", effects.Select(effect =>
            effect.Class + " (" + Word(effect.Reversibility).ToLowerInvariant() + ", " + Place(effect.Place)
            + (effect.By is { } by ? ", by " + by : string.Empty) + ")"));
    }

    public static string Word(EffectReversibility reversibility) => reversibility switch
    {
        EffectReversibility.Reversible => "Reversible",
        EffectReversibility.Compensable => "Compensable",
        _ => "Irreversible",
    };

    public static string Place(EffectPlace place) => place == EffectPlace.External ? "external" : "internal";

    public static string PreviewLine(PreviewEffect effect)
    {
        ArgumentNullException.ThrowIfNull(effect);
        string line = effect.Class + " x" + Number(effect.Count) + " (" + Place(effect.Place) + "): "
            + effect.Operation + " " + effect.Doctype + " in " + effect.Step;
        return effect.Undeclared is { } reason ? line + " - undeclared: " + reason : line;
    }

    public static string TotalLine(EffectClassTotal total)
    {
        ArgumentNullException.ThrowIfNull(total);
        return "Total " + total.Class + " (" + Place(total.Place) + "): " + Number(total.Effects) + " effects, " + Number(total.Count) + " records";
    }

    public static string PreviewHeading(int steps, long undeclared)
    {
        string head = steps == 0
            ? "The test run produced no effects."
            : steps == 1 ? "Effects of 1 step." : "Effects of " + Number(steps) + " steps.";
        return undeclared > 0 ? head + " " + Number(undeclared) + " undeclared." : head;
    }

    // Payloads.
    public static string PayloadNotJson => "The payload is not valid JSON.";

    public static string PayloadNotObject => "The payload must be a JSON object.";

    public static string PayloadUnlistedField => "The payload has a field the contract does not list, and nothing else may cross.";

    public static string PayloadMissingField(string field) => "The required field " + field + " is missing.";

    public static string PayloadWrongKind(string field, ContractFieldKind kind) => "The field " + field + " must be " + kind.Token() + ".";

    public static string PayloadNotWhole(string field) => "The field " + field + " must be a whole number in minor units.";

    public static string PayloadTooLong(string field) => "The field " + field + " is longer than the contract allows.";

    public static string PayloadInvalid(string message) => "The payload for " + message + " does not fit the contract.";

    // Simulation.
    public static string SimulationUsedRealLink =>
        "The server reported a real partner link. The simulation was discarded: a simulation never reaches a partner.";

    public static string DeliveryWord(DeliveryState state) => state switch
    {
        DeliveryState.Queued => "Queued",
        DeliveryState.Delivered => "Delivered",
        DeliveryState.Acknowledged => "Acknowledged",
        DeliveryState.Applied => "Applied",
        DeliveryState.Rejected => "Rejected",
        DeliveryState.TimedOut => "Timed out",
        DeliveryState.Duplicate => "Duplicate",
        _ => "Out of order",
    };

    public static string FailureWord(SimulatedFailure failure) => failure switch
    {
        SimulatedFailure.Reject => "Reject",
        SimulatedFailure.Timeout => "Time out",
        SimulatedFailure.Duplicate => "Send twice",
        SimulatedFailure.OutOfOrder => "Send out of order",
        _ => "No failure",
    };

    public static string EventLine(SimulationEvent entry)
    {
        ArgumentNullException.ThrowIfNull(entry);
        string way = entry.Direction == SimulationDirection.Sent ? "sent to " : "received from ";
        string line = entry.Message + " " + way + entry.Role + ": " + DeliveryWord(entry.State) + " at " + Number(entry.AtSeconds) + "s";
        return entry.Detail is { } detail ? line + " (" + detail + ")" : line;
    }

    public static string SimulationHeading(SimulationResult result)
    {
        ArgumentNullException.ThrowIfNull(result);
        string end = result.Outcome switch
        {
            SimulationOutcome.Completed => "completed",
            SimulationOutcome.Failed => "failed",
            SimulationOutcome.Stalled => "stalled",
            _ => "needs attention",
        };
        string name = result.OutcomeName is { } outcome ? " in " + outcome : string.Empty;
        return "Simulation " + result.RunId + " " + end + name + ".";
    }

    // The workspace.
    public static string WorkspaceName => "Two-organisation process";

    public static string RolesHeading => "Roles";

    public static string MessagesHeading => "Messages";

    public static string OrderHeading => "Order of messages";

    public static string OutcomesHeading => "Outcomes";

    public static string TimeoutsHeading => "Wait limits";

    public static string SignaturesHeading => "Signatures";

    public static string VersionsHeading => "Contract versions";

    public static string ChangesHeading => "Changes since the saved version";

    public static string ProblemsHeading => "Problems";

    public static string DiagramHeading => "State diagram";

    public static string ContractFileHeading => "Contract file";

    public static string ContractFileHelp => "Export fills this box with the contract file to share. Import reads a file a partner sent.";

    public static string SimulatorHeading => "Partner simulator";

    public static string PayloadHeading => "Payload of the selected message";

    public static string SimulationHeadingLabel => "Simulation history";

    public static string EffectsHeading => "Effects";

    public static string NothingYet => "Nothing yet.";

    public static string CheckContract => "Check contract";

    public static string CheckHalf => "Check this half";

    public static string ShowChanges => "Show changes";

    public static string VerifySignatures => "Verify signatures";

    public static string SaveContract => "Save contract";

    public static string Export => "Export";

    public static string Import => "Import";

    public static string RunSimulation => "Run simulation";

    public static string LoadEffects => "Load effects";

    public static string ApplyPayload => "Apply payload";

    public static string UseCanned => "Use canned payload";

    public static string NoContractFile => "Paste a contract file first.";

    // Contract changes.
    public static string ChangeLine(ContractChangeInfo change)
    {
        ArgumentNullException.ThrowIfNull(change);
        string kind = change.Change switch
        {
            ContractChangeKind.Added => "Added",
            ContractChangeKind.Removed => "Removed",
            _ => "Changed",
        };
        string how = change.Compatibility == ContractCompatibility.Breaking ? "breaking" : "additive";
        return kind + " " + change.Subject + " " + change.Name + ": " + how + ". " + change.Detail;
    }

    /// One rendering of each string, for the scan test.
    public static IReadOnlyList<string> Sample()
    {
        LaneInfo own = new("buyer", ParticipantKind.Tenant, true);
        return
        [
            ContractReadOnly, TooLarge, BadName, BadTopic, BadFieldName, BadNumber, AlreadyExists, NotFound, RoleInUse,
            UnknownRole, SelfMessage, MaxLengthOnlyText, OneNext, UnknownReference, ContractFileUnreadable("line 1"),
            FieldNotInContract, MappingOneLine, SecretFieldMapped("f"), RequiredFieldUnmapped("f"), BadExpression("f", "m"),
            ContractOk, ContractProblems(2), ConformanceOk, ConformanceProblems(2), ContractSaved(2),
            SignatureUnchecked, SignatureVerified, SignatureFailed, SignatureOtherVersion,
            LaneLabel(own), MessageLinkLabel(new LaneMessageLink("n", "m", "r", MessageDirection.Out, true)),
            BackEdgeUntyped("a", "b"), ModeOnForwardEdge("a", "b", EdgeMode.Undo), CommitRegionLabel("c"),
            BadgeText(EffectReversibility.Irreversible, false, true), PreviewHeading(2, 1),
            PayloadNotJson, PayloadNotObject, PayloadUnlistedField, PayloadMissingField("f"),
            PayloadWrongKind("f", ContractFieldKind.Whole), PayloadNotWhole("f"), PayloadTooLong("f"), PayloadInvalid("m"),
            SimulationUsedRealLink, DeliveryWord(DeliveryState.TimedOut), FailureWord(SimulatedFailure.OutOfOrder),
            .. Enum.GetValues<EffectReversibility>().Select(Word),
            .. Enum.GetValues<DeliveryState>().Select(DeliveryWord),
            .. Enum.GetValues<SimulatedFailure>().Select(FailureWord),
        ];
    }

    private static string Number(long value) => value.ToString(CultureInfo.InvariantCulture);
}
