using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;

namespace Spotledger.Client.FlowDesigner.Participants;

/// The words of the contract editor's panels: one row per role, message, field, order rule,
/// outcome, wait limit and signature, as a person reads them. The rows are built here, not in the
/// view, so the same text is what the screen shows, what a screen reader announces and what a test
/// asserts. A classified field is flagged in its row: a secret never crosses and says so.
public static class ContractRows
{

    public static IReadOnlyList<string> Participants(ProcessContract contract, string ownRole)
    {
        ArgumentNullException.ThrowIfNull(contract);
        ArgumentNullException.ThrowIfNull(ownRole);
        ParticipantLanes lanes = new(contract, ownRole);
        return
        [
            .. contract.Participants.Select(p => ParticipantStrings.LaneLabel(
                new LaneInfo(p.Role, p.Kind, string.Equals(p.Role, lanes.OwnRole, StringComparison.Ordinal)))),
        ];
    }

    /// The message list: each message, then its fields indented beneath it.
    public static IReadOnlyList<string> Messages(ProcessContract contract, IContractFieldClassifier classifier)
    {
        ArgumentNullException.ThrowIfNull(contract);
        ArgumentNullException.ThrowIfNull(classifier);
        List<string> rows = [];
        foreach (ContractMessage message in contract.Messages)
        {
            rows.Add(message.Name + ": " + message.From + " to " + message.To + " on " + message.Topic
                + ", schema " + message.SchemaVersion.ToString(CultureInfo.InvariantCulture));
            rows.AddRange(message.Fields.Select(field => "  " + Field(field, classifier.Classify(field.Name))));
        }

        return rows;
    }

    public static string Field(ContractField field, ContractFieldClass cls)
    {
        ArgumentNullException.ThrowIfNull(field);
        string text = field.Name + " (" + field.Kind.Token() + (field.Required ? ", required" : ", optional")
            + (field.MaxLength is { } max ? ", up to " + max.ToString(CultureInfo.InvariantCulture) : string.Empty) + ")";
        return cls switch
        {
            ContractFieldClass.Secret => text + " - SECRET, never crosses",
            ContractFieldClass.Personal => text + " - personal data",
            ContractFieldClass.Internal => text + " - internal",
            _ => text,
        };
    }

    /// The order rules as a transition table: after what, who acts, what follows, how often.
    public static IReadOnlyList<string> Order(ProcessContract contract)
    {
        ArgumentNullException.ThrowIfNull(contract);
        return
        [
            .. contract.Order.Select(rule =>
                (rule.After is { } after ? "after " + after : "at the start") + ", " + rule.By
                + (rule.EndsConversation ? " ends in " : " sends ") + rule.Target
                + (rule.Bound is { } bound ? ", at most " + bound.ToString(CultureInfo.InvariantCulture) + " times" : string.Empty)),
        ];
    }

    public static IReadOnlyList<string> Outcomes(ProcessContract contract)
    {
        ArgumentNullException.ThrowIfNull(contract);
        return [.. contract.Outcomes.Select(outcome => outcome.Name + " (" + outcome.Kind.Token().Replace('_', ' ') + ")")];
    }

    public static IReadOnlyList<string> Timeouts(ProcessContract contract)
    {
        ArgumentNullException.ThrowIfNull(contract);
        return
        [
            .. contract.Timeouts.Select(timeout =>
                timeout.By + " waits " + timeout.Seconds.ToString(CultureInfo.InvariantCulture) + "s for " + timeout.Message
                + (timeout.Outcome is { } outcome ? ", then ends in " + outcome : string.Empty)),
        ];
    }

    public static IReadOnlyList<string> Signatures(IEnumerable<SignatureRow> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        return [.. rows.Select(row => row.Role + ": " + row.Algorithm + " key " + row.KeyId + " - " + row.State)];
    }

    public static IReadOnlyList<string> Versions(IEnumerable<ContractVersionSummary> versions)
    {
        ArgumentNullException.ThrowIfNull(versions);
        return
        [
            .. versions.Select(version => "version " + version.Version.ToString(CultureInfo.InvariantCulture)
                + ", " + version.SignatureCount.ToString(CultureInfo.InvariantCulture) + " signatures"),
        ];
    }

    public static IReadOnlyList<string> Changes(IEnumerable<ContractChangeInfo> changes)
    {
        ArgumentNullException.ThrowIfNull(changes);
        return [.. changes.Select(ParticipantStrings.ChangeLine)];
    }

    public static IReadOnlyList<string> Problems(IEnumerable<ContractProblem> problems)
    {
        ArgumentNullException.ThrowIfNull(problems);
        return [.. problems.Select(problem => problem.Code + (problem.Subject.Length > 0 ? " (" + problem.Subject + ")" : string.Empty) + ": " + problem.Message)];
    }

    public static IReadOnlyList<string> Simulator(PartnerSimulator simulator)
    {
        ArgumentNullException.ThrowIfNull(simulator);
        return
        [
            .. simulator.Steps.Select(step => step.Message.Name + " from " + step.FromRole
                + ", " + (step.IsCanned ? "canned payload" : "edited payload")
                + ", delay " + step.DelaySeconds.ToString(CultureInfo.InvariantCulture) + "s"
                + ", " + ParticipantStrings.FailureWord(step.Failure).ToLowerInvariant()),
        ];
    }
}
