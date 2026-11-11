using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Text;
using System.Text.Json;

namespace Spotledger.Client.FlowDesigner.Participants;

/// Builds and checks the payload of a contract message. A payload is a JSON object whose members
/// are exactly the message's fields: nothing else may cross (ADR-058 rule 6), money and quantities
/// are whole numbers in minor units and a fraction or an exponent is refused (ADR-12). A problem
/// names a contract field, never a value, and never a member the payload supplied.
public static class MessagePayload
{

    /// The canned payload: every field filled with a fixed, recognisable value of its kind, in the
    /// contract's field order, so the same message always yields the same bytes.
    public static string Canned(ContractMessage message)
    {
        ArgumentNullException.ThrowIfNull(message);
        using MemoryStream stream = new();
        using (Utf8JsonWriter writer = new(stream))
        {
            writer.WriteStartObject();
            foreach (ContractField field in message.Fields)
            {
                switch (field.Kind)
                {
                    case ContractFieldKind.Whole:
                        writer.WriteNumber(field.Name, 100);
                        break;
                    case ContractFieldKind.Boolean:
                        writer.WriteBoolean(field.Name, true);
                        break;
                    default:
                        string sample = "sample " + field.Name;
                        if (field.MaxLength is { } max && (ulong)sample.Length > max)
                        {
                            sample = sample[..(int)max];
                        }

                        writer.WriteString(field.Name, sample);
                        break;
                }
            }

            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    /// The problems of a payload against its message: empty when the payload may cross.
    public static IReadOnlyList<string> Check(ContractMessage message, string json)
    {
        ArgumentNullException.ThrowIfNull(message);
        ArgumentNullException.ThrowIfNull(json);
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(json, new JsonDocumentOptions { MaxDepth = 4 });
        }
        catch (JsonException)
        {
            return [ParticipantStrings.PayloadNotJson];
        }

        using (document)
        {
            JsonElement root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                return [ParticipantStrings.PayloadNotObject];
            }

            List<string> problems = [];
            HashSet<string> seen = new(StringComparer.Ordinal);
            foreach (JsonProperty property in root.EnumerateObject())
            {
                seen.Add(property.Name);
                ContractField? field = message.Field(property.Name);
                if (field is null)
                {
                    // The member is the payload's own text: it is counted, never echoed.
                    problems.Add(ParticipantStrings.PayloadUnlistedField);
                    continue;
                }

                string? problem = CheckValue(field, property.Value);
                if (problem is not null)
                {
                    problems.Add(problem);
                }
            }

            foreach (ContractField field in message.Fields.Where(f => f.Required && !seen.Contains(f.Name)))
            {
                problems.Add(ParticipantStrings.PayloadMissingField(field.Name));
            }

            return problems;
        }
    }

    private static string? CheckValue(ContractField field, JsonElement value)
    {
        switch (field.Kind)
        {
            case ContractFieldKind.Whole:
                if (value.ValueKind != JsonValueKind.Number)
                {
                    return ParticipantStrings.PayloadWrongKind(field.Name, field.Kind);
                }

                string raw = value.GetRawText();
                bool whole = raw.IndexOfAny(['.', 'e', 'E']) < 0 && long.TryParse(raw, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out _);
                return whole ? null : ParticipantStrings.PayloadNotWhole(field.Name);
            case ContractFieldKind.Boolean:
                return value.ValueKind is JsonValueKind.True or JsonValueKind.False
                    ? null
                    : ParticipantStrings.PayloadWrongKind(field.Name, field.Kind);
            default:
                if (value.ValueKind != JsonValueKind.String)
                {
                    return ParticipantStrings.PayloadWrongKind(field.Name, field.Kind);
                }

                string text = value.GetString() ?? string.Empty;
                return field.MaxLength is { } max && (ulong)text.Length > max
                    ? ParticipantStrings.PayloadTooLong(field.Name)
                    : null;
        }
    }
}

/// One message the simulated partner plays and the choices a person made for it.
public sealed record PartnerStep(
    ContractMessage Message,
    string PayloadJson,
    bool IsCanned,
    long DelaySeconds,
    SimulatedFailure Failure)
{

    public string FromRole => Message.From;
}

/// The plan of a simulation: the messages the other lanes send, each with a payload (canned or
/// edited), a delay and a failure. The plan is plain data and reads no clock: a delay is a number of
/// whole seconds of simulated time.
public sealed class PartnerSimulator
{

    public const long MaxDelaySeconds = 365L * 24 * 3600;

    private readonly Dictionary<string, PartnerStep> _steps = new(StringComparer.Ordinal);

    private readonly List<string> _order = [];

    public PartnerSimulator(ProcessContract contract, string ownRole)
    {
        ArgumentNullException.ThrowIfNull(contract);
        ArgumentNullException.ThrowIfNull(ownRole);
        Contract = contract;
        OwnRole = ownRole;
        Reset();
    }

    public ProcessContract Contract { get; private set; }

    public string OwnRole { get; }

    /// The partner's steps, in the contract's order of messages.
    public IReadOnlyList<PartnerStep> Steps => [.. _order.Select(name => _steps[name])];

    public PartnerStep? Step(string message)
    {
        ArgumentNullException.ThrowIfNull(message);
        return _steps.GetValueOrDefault(message);
    }

    /// Rebuilds the plan from a (possibly edited) contract. Choices made for a message that still
    /// exists, still comes from another lane and still has a payload that fits are kept.
    public void UseContract(ProcessContract contract)
    {
        ArgumentNullException.ThrowIfNull(contract);
        Dictionary<string, PartnerStep> before = new(_steps, StringComparer.Ordinal);
        Contract = contract;
        Reset();
        foreach (KeyValuePair<string, PartnerStep> pair in before)
        {
            ContractMessage? message = contract.Message(pair.Key);
            if (message is null || !_steps.ContainsKey(pair.Key))
            {
                continue;
            }

            PartnerStep old = pair.Value;
            bool keep = !old.IsCanned && MessagePayload.Check(message, old.PayloadJson).Count == 0;
            string payload = keep ? old.PayloadJson : MessagePayload.Canned(message);
            _steps[pair.Key] = new PartnerStep(message, payload, !keep, old.DelaySeconds, old.Failure);
        }
    }

    /// Goes back to the canned payload for a message.
    public bool UseCanned(string message)
    {
        ArgumentNullException.ThrowIfNull(message);
        if (!_steps.TryGetValue(message, out PartnerStep? step))
        {
            return false;
        }

        _steps[message] = step with { PayloadJson = MessagePayload.Canned(step.Message), IsCanned = true };
        return true;
    }

    /// Sets an edited payload. A payload the contract's field list would not let cross is refused,
    /// so the simulator can never send what a real partner could not.
    public bool SetPayload(string message, string json, out IReadOnlyList<string> problems)
    {
        ArgumentNullException.ThrowIfNull(message);
        ArgumentNullException.ThrowIfNull(json);
        if (!_steps.TryGetValue(message, out PartnerStep? step))
        {
            problems = [ParticipantStrings.NotFound];
            return false;
        }

        problems = MessagePayload.Check(step.Message, json);
        if (problems.Count > 0)
        {
            return false;
        }

        _steps[message] = step with { PayloadJson = json, IsCanned = false };
        return true;
    }

    public bool SetDelay(string message, long seconds)
    {
        ArgumentNullException.ThrowIfNull(message);
        if (seconds < 0 || seconds > MaxDelaySeconds || !_steps.TryGetValue(message, out PartnerStep? step))
        {
            return false;
        }

        _steps[message] = step with { DelaySeconds = seconds };
        return true;
    }

    public bool SetFailure(string message, SimulatedFailure failure)
    {
        ArgumentNullException.ThrowIfNull(message);
        if (!_steps.TryGetValue(message, out PartnerStep? step))
        {
            return false;
        }

        _steps[message] = step with { Failure = failure };
        return true;
    }

    /// The request for the server. It names the process half, the contract and the partner's
    /// choices, and nothing that addresses a real link.
    public SimulationRequest ToRequest(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        return new SimulationRequest(
            document.Name,
            document.Version,
            document.ToText(),
            Contract.ToText(),
            OwnRole,
            [.. Steps.Select(step => new SimulatedMessage(step.Message.Name, step.FromRole, step.PayloadJson, step.DelaySeconds, step.Failure))]);
    }

    private void Reset()
    {
        _steps.Clear();
        _order.Clear();
        foreach (ContractMessage message in Contract.Messages.Where(m => !string.Equals(m.From, OwnRole, StringComparison.Ordinal)))
        {
            _order.Add(message.Name);
            _steps[message.Name] = new PartnerStep(message, MessagePayload.Canned(message), true, 0, SimulatedFailure.None);
        }
    }
}

/// One row of the execution history of a simulation.
public sealed record SimulationRow(int Sequence, string Text, DeliveryState State, SimulationDirection Direction, string Message);

/// The execution history of a simulation: each message sent or received and the delivery state it
/// reached, in the server's order.
public sealed class SimulationHistory
{

    public SimulationHistory(SimulationResult result)
    {
        ArgumentNullException.ThrowIfNull(result);
        Result = result;
        Rows =
        [
            .. result.Events
                .OrderBy(entry => entry.Sequence)
                .Select(entry => new SimulationRow(entry.Sequence, ParticipantStrings.EventLine(entry), entry.State, entry.Direction, entry.Message)),
        ];
    }

    public SimulationResult Result { get; }

    public IReadOnlyList<SimulationRow> Rows { get; }

    public string Heading => ParticipantStrings.SimulationHeading(Result);

    public IReadOnlyList<SimulationRow> InState(DeliveryState state) => [.. Rows.Where(row => row.State == state)];

    public string ToText()
    {
        StringBuilder builder = new();
        builder.Append(Heading).Append('\n');
        foreach (SimulationRow row in Rows)
        {
            builder.Append(row.Sequence.ToString(CultureInfo.InvariantCulture)).Append(". ").Append(row.Text).Append('\n');
        }

        return builder.ToString();
    }
}
