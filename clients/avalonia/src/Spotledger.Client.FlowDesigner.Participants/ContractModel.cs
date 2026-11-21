using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner.Participants;

/// Whether a participant is a platform tenant or an external system (ADR-080 section 5.2). The
/// tokens are the contract text form's.
public enum ParticipantKind
{
    Tenant,
    External,
}

/// The kind of value a contract field may carry. There is no floating-point kind: money and
/// quantities cross as integers in minor units (ADR-12).
public enum ContractFieldKind
{
    Text,
    Whole,
    Boolean,
}

/// How the field metadata classifies a field. Only the server knows it, so the designer asks for
/// it through <see cref="IContractFieldClassifier"/>; a secret never crosses and is flagged.
public enum ContractFieldClass
{
    Public,
    Internal,
    Personal,
    Secret,
}

/// How a conversation ends.
public enum ContractOutcomeKind
{
    Completed,
    Cancelled,
    Failed,
    NeedsAttention,
}

/// The text tokens of the contract text form, exactly as <c>spot-bpm-contract</c> spells them.
public static class ContractTokens
{

    public static string Token(this ParticipantKind kind) => kind switch
    {
        ParticipantKind.Tenant => "tenant",
        ParticipantKind.External => "external",
        _ => throw new ArgumentOutOfRangeException(nameof(kind)),
    };

    public static string Token(this ContractFieldKind kind) => kind switch
    {
        ContractFieldKind.Text => "string",
        ContractFieldKind.Whole => "integer",
        ContractFieldKind.Boolean => "boolean",
        _ => throw new ArgumentOutOfRangeException(nameof(kind)),
    };

    public static string Token(this ContractOutcomeKind kind) => kind switch
    {
        ContractOutcomeKind.Completed => "completed",
        ContractOutcomeKind.Cancelled => "cancelled",
        ContractOutcomeKind.Failed => "failed",
        ContractOutcomeKind.NeedsAttention => "needs_attention",
        _ => throw new ArgumentOutOfRangeException(nameof(kind)),
    };

    public static bool TryParse(string? token, out ParticipantKind kind)
    {
        (bool ok, kind) = token switch
        {
            "tenant" => (true, ParticipantKind.Tenant),
            "external" => (true, ParticipantKind.External),
            _ => (false, ParticipantKind.Tenant),
        };
        return ok;
    }

    public static bool TryParse(string? token, out ContractFieldKind kind)
    {
        (bool ok, kind) = token switch
        {
            "string" => (true, ContractFieldKind.Text),
            "integer" => (true, ContractFieldKind.Whole),
            "boolean" => (true, ContractFieldKind.Boolean),
            _ => (false, ContractFieldKind.Text),
        };
        return ok;
    }

    public static bool TryParse(string? token, out ContractOutcomeKind kind)
    {
        (bool ok, kind) = token switch
        {
            "completed" => (true, ContractOutcomeKind.Completed),
            "cancelled" => (true, ContractOutcomeKind.Cancelled),
            "failed" => (true, ContractOutcomeKind.Failed),
            "needs_attention" => (true, ContractOutcomeKind.NeedsAttention),
            _ => (false, ContractOutcomeKind.Completed),
        };
        return ok;
    }
}

/// One role of the contract.
public sealed record ContractParticipant(string Role, ParticipantKind Kind);

/// One field of a message: the only shape in which a value may cross (ADR-058 rule 6).
public sealed record ContractField(string Name, ContractFieldKind Kind, bool Required = true, ulong? MaxLength = null);

/// One message the participants exchange.
public sealed record ContractMessage(
    string Name,
    string Topic,
    int SchemaVersion,
    string From,
    string To,
    IReadOnlyList<ContractField> Fields)
{

    public ContractField? Field(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        return Fields.FirstOrDefault(field => string.Equals(field.Name, name, StringComparison.Ordinal));
    }
}

/// One order rule: after a message (or at the start), one role may send the next message, or the
/// conversation ends in an outcome. Exactly one of <see cref="NextMessage"/> and
/// <see cref="NextOutcome"/> is set.
public sealed record ContractTransition(
    string By,
    string? After,
    string? NextMessage,
    string? NextOutcome,
    int? Bound = null)
{

    public string Target => NextMessage ?? NextOutcome ?? string.Empty;

    public bool EndsConversation => NextOutcome is not null;
}

/// One outcome state of the whole conversation.
public sealed record ContractOutcome(string Name, ContractOutcomeKind Kind);

/// The time limit of one wait, in whole seconds (ADR-12: never a float).
public sealed record ContractTimeout(string Message, string By, long Seconds, string? Outcome = null);

/// One participant's signature on a contract version. The designer only displays it; whether the
/// signature verifies is the server's answer.
public sealed record ContractSignature(string Role, string Algorithm, string KeyId, string SignedHash, string Signature);

/// A versioned process contract: the agreed document both halves of a conversation hold. The
/// lists are read-only views; <see cref="ContractEditor"/> replaces the record on every edit, so a
/// value taken earlier never changes under a reader.
public sealed record ProcessContract(
    string Id,
    int Version,
    string? Title,
    IReadOnlyList<ContractParticipant> Participants,
    IReadOnlyList<ContractMessage> Messages,
    IReadOnlyList<ContractTransition> Order,
    IReadOnlyList<ContractOutcome> Outcomes,
    IReadOnlyList<ContractTimeout> Timeouts,
    IReadOnlyList<ContractSignature> Signatures)
{

    public static ProcessContract Empty(string id) =>
        new(id, 1, null, [], [], [], [], [], []);

    public ContractParticipant? Participant(string role)
    {
        ArgumentNullException.ThrowIfNull(role);
        return Participants.FirstOrDefault(p => string.Equals(p.Role, role, StringComparison.Ordinal));
    }

    public ContractMessage? Message(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        return Messages.FirstOrDefault(m => string.Equals(m.Name, name, StringComparison.Ordinal));
    }

    public ContractOutcome? Outcome(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        return Outcomes.FirstOrDefault(o => string.Equals(o.Name, name, StringComparison.Ordinal));
    }

    public ContractTimeout? Timeout(string message, string by)
    {
        ArgumentNullException.ThrowIfNull(message);
        ArgumentNullException.ThrowIfNull(by);
        return Timeouts.FirstOrDefault(t =>
            string.Equals(t.Message, message, StringComparison.Ordinal) && string.Equals(t.By, by, StringComparison.Ordinal));
    }

    /// The roles other than <paramref name="ownRole"/>, in definition order: the lanes whose
    /// insides the designer hides.
    public IReadOnlyList<ContractParticipant> OtherParticipants(string ownRole)
    {
        ArgumentNullException.ThrowIfNull(ownRole);
        return [.. Participants.Where(p => !string.Equals(p.Role, ownRole, StringComparison.Ordinal))];
    }

    /// The messages a role sends, in definition order.
    public IReadOnlyList<ContractMessage> SentBy(string role)
    {
        ArgumentNullException.ThrowIfNull(role);
        return [.. Messages.Where(m => string.Equals(m.From, role, StringComparison.Ordinal))];
    }

    /// The messages a role receives, in definition order.
    public IReadOnlyList<ContractMessage> ReceivedBy(string role)
    {
        ArgumentNullException.ThrowIfNull(role);
        return [.. Messages.Where(m => string.Equals(m.To, role, StringComparison.Ordinal))];
    }

    /// The canonical text form: the same bytes the server would write.
    public string ToText() => ContractText.Write(this);

    public static ProcessContract FromText(string text) => ContractText.Read(text);
}

/// The limits of the contract text form, the same numbers <c>spot-bpm-contract</c> enforces, so a
/// contract the designer accepts is one the server accepts.
public static class ContractLimits
{

    public const int MaxTextBytes = 256 * 1024;

    public const int MaxRoles = 32;

    public const int MaxMessages = 256;

    public const int MaxFieldsPerMessage = 64;

    public const int MaxOrderRules = 1_024;

    public const int MaxOutcomes = 64;

    public const int MaxTimeouts = 256;

    public const int MaxSignatures = 64;
}

/// Asks the field metadata how a field is classified. The server is authoritative; the designer
/// flags what it is told. A host binds this to the typed client; a host that does not track classes
/// passes <see cref="PublicFields"/>.
public interface IContractFieldClassifier
{
    ContractFieldClass Classify(string field);
}

/// Classifies every field as public.
public sealed class PublicFields : IContractFieldClassifier
{

    public static PublicFields Instance { get; } = new();

    public ContractFieldClass Classify(string field) => ContractFieldClass.Public;
}

/// A classifier built from a plain table, for a host with metadata in hand and for tests.
public sealed class TableFieldClassifier(IReadOnlyDictionary<string, ContractFieldClass> classes) : IContractFieldClassifier
{

    public ContractFieldClass Classify(string field)
    {
        ArgumentNullException.ThrowIfNull(field);
        return classes.TryGetValue(field, out ContractFieldClass found) ? found : ContractFieldClass.Public;
    }
}
