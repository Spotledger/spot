using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner.Participants;

/// The outcome of one contract edit: done, or why it was refused. A refused edit changes nothing.
public sealed record ContractEditResult(bool Ok, string? Error = null)
{

    public static ContractEditResult Done { get; } = new(true);

    public static ContractEditResult Refused(string error) => new(false, error);
}

/// The contract editor's model: the contract a person is editing in the side panel, with undo and
/// redo, the local grammar of every name, the file export and import, and the version bump. The
/// meaning of a contract (an unreachable message, a dead end, an unbounded cycle) is the server's to
/// judge; this editor only keeps a person from writing something the text form cannot hold.
///
/// Every edit replaces the contract record, so a value read earlier never changes under a reader.
public sealed class ContractEditor
{

    public const int MaxHistory = 200;

    private readonly List<ProcessContract> _undo = [];

    private readonly List<ProcessContract> _redo = [];

    public ContractEditor(ProcessContract contract, IContractFieldClassifier? classifier = null)
    {
        ArgumentNullException.ThrowIfNull(contract);
        Current = contract;
        Baseline = contract;
        Classifier = classifier ?? PublicFields.Instance;
    }

    public ProcessContract Current { get; private set; }

    /// The version last loaded from or saved to the server: the left side of a version diff.
    public ProcessContract Baseline { get; private set; }

    public IContractFieldClassifier Classifier { get; }

    public bool ReadOnly { get; set; }

    public bool CanUndo => _undo.Count > 0;

    public bool CanRedo => _redo.Count > 0;

    public bool IsModified => !string.Equals(Current.ToText(), Baseline.ToText(), StringComparison.Ordinal);

    public event EventHandler? Changed;

    /// The canonical text a person shares with a partner.
    public string Export() => Current.ToText();

    /// Replaces the contract with one read from a file a partner sent. The text is checked for shape
    /// only; the server judges the rest. A text that does not read changes nothing.
    public ContractEditResult Import(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (ReadOnly)
        {
            return ContractEditResult.Refused(ParticipantStrings.ContractReadOnly);
        }

        try
        {
            return Replace(ProcessContract.FromText(text));
        }
        catch (FlowTextException error)
        {
            return ContractEditResult.Refused(ParticipantStrings.ContractFileUnreadable(error.Message));
        }
    }

    /// Marks the contract as saved at the server: the baseline becomes the current version.
    public void MarkSaved(int version)
    {
        Current = Current with { Version = version };
        Baseline = Current;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    /// Starts the next version: the number goes up and the signatures go (a signature covers one
    /// version's content hash, so a changed contract must be signed again). A running conversation
    /// stays on the version it started with.
    public ContractEditResult NewVersion()
    {
        if (Current.Version >= int.MaxValue)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        return Replace(Current with { Version = Current.Version + 1, Signatures = [] });
    }

    public ContractEditResult SetTitle(string? title) =>
        Replace(Current with { Title = string.IsNullOrWhiteSpace(title) ? null : title.Trim() });

    public ContractEditResult AddParticipant(string role, ParticipantKind kind)
    {
        if (!FlowId.IsName(role))
        {
            return ContractEditResult.Refused(ParticipantStrings.BadName);
        }

        if (Current.Participant(role) is not null)
        {
            return ContractEditResult.Refused(ParticipantStrings.AlreadyExists);
        }

        if (Current.Participants.Count >= ContractLimits.MaxRoles)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        return Replace(Current with { Participants = [.. Current.Participants, new ContractParticipant(role, kind)] });
    }

    public ContractEditResult RemoveParticipant(string role)
    {
        if (Current.Participant(role) is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        if (Current.Messages.Any(m => m.From == role || m.To == role)
            || Current.Order.Any(t => t.By == role)
            || Current.Timeouts.Any(t => t.By == role))
        {
            return ContractEditResult.Refused(ParticipantStrings.RoleInUse);
        }

        return Replace(Current with
        {
            Participants = [.. Current.Participants.Where(p => p.Role != role)],
            Signatures = [.. Current.Signatures.Where(s => s.Role != role)],
        });
    }

    public ContractEditResult AddMessage(string name, string topic, string from, string to)
    {
        if (!FlowId.IsName(name))
        {
            return ContractEditResult.Refused(ParticipantStrings.BadName);
        }

        if (!IsTopic(topic))
        {
            return ContractEditResult.Refused(ParticipantStrings.BadTopic);
        }

        if (Current.Message(name) is not null)
        {
            return ContractEditResult.Refused(ParticipantStrings.AlreadyExists);
        }

        if (Current.Participant(from) is null || Current.Participant(to) is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.UnknownRole);
        }

        if (string.Equals(from, to, StringComparison.Ordinal))
        {
            return ContractEditResult.Refused(ParticipantStrings.SelfMessage);
        }

        if (Current.Messages.Count >= ContractLimits.MaxMessages)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        return Replace(Current with { Messages = [.. Current.Messages, new ContractMessage(name, topic, 1, from, to, [])] });
    }

    /// Removes a message and everything that names it: its order rules and its wait limits.
    public ContractEditResult RemoveMessage(string name)
    {
        if (Current.Message(name) is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        return Replace(Current with
        {
            Messages = [.. Current.Messages.Where(m => m.Name != name)],
            Order = [.. Current.Order.Where(t => t.After != name && t.NextMessage != name)],
            Timeouts = [.. Current.Timeouts.Where(t => t.Message != name)],
        });
    }

    public ContractEditResult SetSchemaVersion(string message, int schemaVersion)
    {
        if (schemaVersion < 1)
        {
            return ContractEditResult.Refused(ParticipantStrings.BadNumber);
        }

        return ReplaceMessage(message, m => m with { SchemaVersion = schemaVersion });
    }

    public ContractEditResult AddField(string message, string name, ContractFieldKind kind, bool required = true, ulong? maxLength = null)
    {
        if (!MessageMapping.IsFieldName(name))
        {
            return ContractEditResult.Refused(ParticipantStrings.BadFieldName);
        }

        if (maxLength is not null && kind != ContractFieldKind.Text)
        {
            return ContractEditResult.Refused(ParticipantStrings.MaxLengthOnlyText);
        }

        ContractMessage? target = Current.Message(message);
        if (target is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        if (target.Field(name) is not null)
        {
            return ContractEditResult.Refused(ParticipantStrings.AlreadyExists);
        }

        if (target.Fields.Count >= ContractLimits.MaxFieldsPerMessage)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        return ReplaceMessage(message, m => m with { Fields = [.. m.Fields, new ContractField(name, kind, required, maxLength)] });
    }

    public ContractEditResult RemoveField(string message, string name)
    {
        ContractMessage? target = Current.Message(message);
        if (target?.Field(name) is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        return ReplaceMessage(message, m => m with { Fields = [.. m.Fields.Where(f => f.Name != name)] });
    }

    public ContractEditResult SetFieldRequired(string message, string name, bool required)
    {
        ContractMessage? target = Current.Message(message);
        if (target?.Field(name) is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        return ReplaceMessage(message, m => m with
        {
            Fields = [.. m.Fields.Select(f => f.Name == name ? f with { Required = required } : f)],
        });
    }

    /// Adds an order rule. <paramref name="after"/> is null for a message that starts a
    /// conversation; exactly one of <paramref name="nextMessage"/> and <paramref name="nextOutcome"/>
    /// is given.
    public ContractEditResult AddTransition(string by, string? after, string? nextMessage, string? nextOutcome, int? bound = null)
    {
        if ((nextMessage is null) == (nextOutcome is null))
        {
            return ContractEditResult.Refused(ParticipantStrings.OneNext);
        }

        if (Current.Participant(by) is null
            || (after is not null && Current.Message(after) is null)
            || (nextMessage is not null && Current.Message(nextMessage) is null)
            || (nextOutcome is not null && Current.Outcome(nextOutcome) is null))
        {
            return ContractEditResult.Refused(ParticipantStrings.UnknownReference);
        }

        if (bound is < 1)
        {
            return ContractEditResult.Refused(ParticipantStrings.BadNumber);
        }

        ContractTransition added = new(by, after, nextMessage, nextOutcome, bound);
        if (Current.Order.Any(t => t.By == by && t.After == after && t.NextMessage == nextMessage && t.NextOutcome == nextOutcome))
        {
            return ContractEditResult.Refused(ParticipantStrings.AlreadyExists);
        }

        if (Current.Order.Count >= ContractLimits.MaxOrderRules)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        return Replace(Current with { Order = [.. Current.Order, added] });
    }

    public ContractEditResult RemoveTransition(int index)
    {
        if (index < 0 || index >= Current.Order.Count)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        return Replace(Current with { Order = [.. Current.Order.Where((_, i) => i != index)] });
    }

    public ContractEditResult AddOutcome(string name, ContractOutcomeKind kind)
    {
        if (!FlowId.IsName(name))
        {
            return ContractEditResult.Refused(ParticipantStrings.BadName);
        }

        if (Current.Outcome(name) is not null)
        {
            return ContractEditResult.Refused(ParticipantStrings.AlreadyExists);
        }

        if (Current.Outcomes.Count >= ContractLimits.MaxOutcomes)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        return Replace(Current with { Outcomes = [.. Current.Outcomes, new ContractOutcome(name, kind)] });
    }

    /// Sets (or replaces) the time limit of a wait, in whole seconds.
    public ContractEditResult SetTimeout(string message, string by, long seconds, string? outcome = null)
    {
        if (seconds < 1)
        {
            return ContractEditResult.Refused(ParticipantStrings.BadNumber);
        }

        if (Current.Message(message) is null || Current.Participant(by) is null
            || (outcome is not null && Current.Outcome(outcome) is null))
        {
            return ContractEditResult.Refused(ParticipantStrings.UnknownReference);
        }

        List<ContractTimeout> timeouts = [.. Current.Timeouts.Where(t => !(t.Message == message && t.By == by))];
        if (timeouts.Count >= ContractLimits.MaxTimeouts)
        {
            return ContractEditResult.Refused(ParticipantStrings.TooLarge);
        }

        timeouts.Add(new ContractTimeout(message, by, seconds, outcome));
        return Replace(Current with { Timeouts = timeouts });
    }

    public bool Undo()
    {
        if (_undo.Count == 0)
        {
            return false;
        }

        _redo.Add(Current);
        Current = _undo[^1];
        _undo.RemoveAt(_undo.Count - 1);
        Changed?.Invoke(this, EventArgs.Empty);
        return true;
    }

    public bool Redo()
    {
        if (_redo.Count == 0)
        {
            return false;
        }

        _undo.Add(Current);
        Current = _redo[^1];
        _redo.RemoveAt(_redo.Count - 1);
        Changed?.Invoke(this, EventArgs.Empty);
        return true;
    }

    /// The fields of the contract the metadata classifies as secret: the ones a person is warned
    /// about before the server says so.
    public IReadOnlyList<(string Message, string Field)> SecretFields() =>
    [
        .. Current.Messages.SelectMany(message => message.Fields
            .Where(field => Classifier.Classify(field.Name) == ContractFieldClass.Secret)
            .Select(field => (message.Name, field.Name))),
    ];

    /// A field's class for display: public unless the metadata says otherwise.
    public ContractFieldClass ClassOf(string field) => Classifier.Classify(field);

    internal static bool IsTopic(string topic)
    {
        if (string.IsNullOrEmpty(topic) || topic.Length > 128)
        {
            return false;
        }

        string[] segments = topic.Split('.');
        return segments.Length >= 2 && segments.All(IsSegment);
    }

    private static bool IsSegment(string segment)
    {
        if (segment.Length == 0 || segment[0] < 'a' || segment[0] > 'z')
        {
            return false;
        }

        return segment.All(c => (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_');
    }

    private ContractEditResult ReplaceMessage(string name, Func<ContractMessage, ContractMessage> change)
    {
        if (Current.Message(name) is null)
        {
            return ContractEditResult.Refused(ParticipantStrings.NotFound);
        }

        return Replace(Current with
        {
            Messages = [.. Current.Messages.Select(m => m.Name == name ? change(m) : m)],
        });
    }

    private ContractEditResult Replace(ProcessContract next)
    {
        if (ReadOnly)
        {
            return ContractEditResult.Refused(ParticipantStrings.ContractReadOnly);
        }

        _undo.Add(Current);
        if (_undo.Count > MaxHistory)
        {
            _undo.RemoveAt(0);
        }

        _redo.Clear();
        Current = next;
        Changed?.Invoke(this, EventArgs.Empty);
        return ContractEditResult.Done;
    }
}
