using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;

namespace Spotledger.Client.FlowDesigner.Participants;

/// The contract text form (ADR-080 section 5.2): one reviewable TOML document per contract
/// version, which is what two participants exchange. The writer is canonical (a fixed key order,
/// one scalar per line, lists in definition order), so the same contract always renders to the same
/// bytes and an export followed by an import is the identity.
///
/// The reader checks shapes only: a missing key, a wrong type, an unknown key, the
/// <c>next</c>/<c>outcome</c> pair and the size bounds. Meaning is the server's to judge
/// (<see cref="IParticipantServer.CheckContractAsync"/>). A top-level section this build does not
/// know is ignored, so adding one later is a pure addition. An error never echoes the document's
/// own text: it carries a line number and a fixed phrase.
public static class ContractText
{

    private const string Header =
        "# Spotledger process contract (spot-bpm-contract). Canonical: one scalar per line, a fixed key order.";

    public static string Write(ProcessContract contract)
    {
        ArgumentNullException.ThrowIfNull(contract);
        StringBuilder builder = new();
        builder.Append(Header).Append('\n');
        Line(builder, "id", ProcessText.Quote(contract.Id));
        Line(builder, "version", contract.Version.ToString(CultureInfo.InvariantCulture));
        if (contract.Title is { } title)
        {
            Line(builder, "title", ProcessText.Quote(title));
        }

        foreach (ContractParticipant participant in contract.Participants)
        {
            Table(builder, "participants");
            Line(builder, "role", ProcessText.Quote(participant.Role));
            Line(builder, "kind", ProcessText.Quote(participant.Kind.Token()));
        }

        foreach (ContractMessage message in contract.Messages)
        {
            Table(builder, "messages");
            Line(builder, "name", ProcessText.Quote(message.Name));
            Line(builder, "topic", ProcessText.Quote(message.Topic));
            Line(builder, "schema_version", message.SchemaVersion.ToString(CultureInfo.InvariantCulture));
            Line(builder, "from", ProcessText.Quote(message.From));
            Line(builder, "to", ProcessText.Quote(message.To));
            foreach (ContractField field in message.Fields)
            {
                Table(builder, "messages.fields");
                Line(builder, "name", ProcessText.Quote(field.Name));
                Line(builder, "kind", ProcessText.Quote(field.Kind.Token()));
                Line(builder, "required", field.Required ? "true" : "false");
                if (field.MaxLength is { } max)
                {
                    Line(builder, "max_len", max.ToString(CultureInfo.InvariantCulture));
                }
            }
        }

        foreach (ContractTransition transition in contract.Order)
        {
            Table(builder, "order");
            Line(builder, "by", ProcessText.Quote(transition.By));
            if (transition.After is { } after)
            {
                Line(builder, "after", ProcessText.Quote(after));
            }

            Line(builder, transition.EndsConversation ? "outcome" : "next", ProcessText.Quote(transition.Target));
            if (transition.Bound is { } bound)
            {
                Line(builder, "bound", bound.ToString(CultureInfo.InvariantCulture));
            }
        }

        foreach (ContractOutcome outcome in contract.Outcomes)
        {
            Table(builder, "outcomes");
            Line(builder, "name", ProcessText.Quote(outcome.Name));
            Line(builder, "kind", ProcessText.Quote(outcome.Kind.Token()));
        }

        foreach (ContractTimeout timeout in contract.Timeouts)
        {
            Table(builder, "timeouts");
            Line(builder, "message", ProcessText.Quote(timeout.Message));
            Line(builder, "by", ProcessText.Quote(timeout.By));
            Line(builder, "seconds", timeout.Seconds.ToString(CultureInfo.InvariantCulture));
            if (timeout.Outcome is { } outcome)
            {
                Line(builder, "outcome", ProcessText.Quote(outcome));
            }
        }

        foreach (ContractSignature signature in contract.Signatures)
        {
            Table(builder, "signatures");
            Line(builder, "role", ProcessText.Quote(signature.Role));
            Line(builder, "algorithm", ProcessText.Quote(signature.Algorithm));
            Line(builder, "key_id", ProcessText.Quote(signature.KeyId));
            Line(builder, "signed_hash", ProcessText.Quote(signature.SignedHash));
            Line(builder, "signature", ProcessText.Quote(signature.Signature));
        }

        return builder.ToString();
    }

    public static ProcessContract Read(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (Encoding.UTF8.GetByteCount(text) > ContractLimits.MaxTextBytes)
        {
            throw FlowTextException.AtLimit(0, "the contract text is larger", "256 KiB limit");
        }

        Reader reader = new();
        int number = 0;
        foreach (string raw in text.Replace("\r\n", "\n", StringComparison.Ordinal).Split('\n'))
        {
            number++;
            string line = raw.Trim();
            if (line.Length == 0 || line[0] == '#')
            {
                continue;
            }

            if (line[0] == '[')
            {
                reader.OpenTable(line, number);
                continue;
            }

            int equals = line.IndexOf('=', StringComparison.Ordinal);
            if (equals <= 0)
            {
                throw FlowTextException.At(number, "a line is neither a table header nor a key and a value");
            }

            reader.Set(line[..equals].Trim(), ProcessText.ParseLiteral(line[(equals + 1)..], number), number);
        }

        return reader.Build();
    }

    private static void Table(StringBuilder builder, string name) =>
        builder.Append('\n').Append("[[").Append(name).Append("]]\n");

    private static void Line(StringBuilder builder, string key, string value) =>
        builder.Append(key).Append(" = ").Append(value).Append('\n');

    private sealed class Reader
    {

        private readonly Dictionary<string, (FlowValue Value, int Line)> _top = new(StringComparer.Ordinal);

        private readonly List<Row> _participants = [];

        private readonly List<MessageRow> _messages = [];

        private readonly List<Row> _order = [];

        private readonly List<Row> _outcomes = [];

        private readonly List<Row> _timeouts = [];

        private readonly List<Row> _signatures = [];

        private Row? _current;

        public void OpenTable(string header, int line)
        {
            if (!header.StartsWith("[[", StringComparison.Ordinal) || !header.EndsWith("]]", StringComparison.Ordinal))
            {
                throw FlowTextException.At(line, "only [[array]] tables are part of the contract form");
            }

            string name = header[2..^2].Trim();
            Row row = new(line);
            switch (name)
            {
                case "participants":
                    Add(_participants, row, ContractLimits.MaxRoles, line);
                    break;
                case "messages":
                    MessageRow message = new(line);
                    Add(_messages, message, ContractLimits.MaxMessages, line);
                    row = message;
                    break;
                case "messages.fields":
                    if (_messages.Count == 0)
                    {
                        throw FlowTextException.At(line, "a field table comes before any message");
                    }

                    MessageRow owner = _messages[^1];
                    Add(owner.Fields, row, ContractLimits.MaxFieldsPerMessage, line);
                    break;
                case "order":
                    Add(_order, row, ContractLimits.MaxOrderRules, line);
                    break;
                case "outcomes":
                    Add(_outcomes, row, ContractLimits.MaxOutcomes, line);
                    break;
                case "timeouts":
                    Add(_timeouts, row, ContractLimits.MaxTimeouts, line);
                    break;
                case "signatures":
                    Add(_signatures, row, ContractLimits.MaxSignatures, line);
                    break;
                default:
                    // A section this build does not know: its keys are read and dropped.
                    row = new Row(line);
                    break;
            }

            _current = row;
        }

        public void Set(string key, FlowValue value, int line)
        {
            if (_current is null)
            {
                if (_top.ContainsKey(key))
                {
                    throw FlowTextException.At(line, "a key is set twice");
                }

                _top[key] = (value, line);
                return;
            }

            if (_current.Values.ContainsKey(key))
            {
                throw FlowTextException.At(line, "a key is set twice");
            }

            _current.Values[key] = (value, line);
        }

        public ProcessContract Build()
        {
            foreach (KeyValuePair<string, (FlowValue Value, int Line)> pair in _top)
            {
                if (pair.Key is not ("id" or "version" or "title"))
                {
                    throw FlowTextException.AtUnknownKey(pair.Value.Line, ["id", "version", "title"]);
                }
            }

            string id = Text(_top, "id", 0);
            int version = (int)Whole(_top, "version", 0, 1, int.MaxValue);
            string? title = OptionalText(_top, "title");
            return new ProcessContract(
                id,
                version,
                title,
                [.. _participants.Select(Participant)],
                [.. _messages.Select(Message)],
                [.. _order.Select(Transition)],
                [.. _outcomes.Select(Outcome)],
                [.. _timeouts.Select(Timeout)],
                [.. _signatures.Select(Signature)]);
        }

        private static void Add<T>(List<T> list, T item, int limit, int line)
        {
            if (list.Count >= limit)
            {
                throw FlowTextException.AtLimit(line, "the contract has more entries in one table", "limit of the contract form");
            }

            list.Add(item);
        }

        private static ContractParticipant Participant(Row row)
        {
            Allow(row, ["role", "kind"]);
            string kind = Text(row.Values, "kind", row.Line);
            if (!ContractTokens.TryParse(kind, out ParticipantKind parsed))
            {
                throw FlowTextException.AtKey(row.Line, "kind", "is not a participant kind");
            }

            return new ContractParticipant(Text(row.Values, "role", row.Line), parsed);
        }

        private static ContractMessage Message(MessageRow row)
        {
            Allow(row, ["name", "topic", "schema_version", "from", "to"]);
            return new ContractMessage(
                Text(row.Values, "name", row.Line),
                Text(row.Values, "topic", row.Line),
                (int)Whole(row.Values, "schema_version", row.Line, 0, int.MaxValue),
                Text(row.Values, "from", row.Line),
                Text(row.Values, "to", row.Line),
                [.. row.Fields.Select(Field)]);
        }

        private static ContractField Field(Row row)
        {
            Allow(row, ["name", "kind", "required", "max_len"]);
            string kind = Text(row.Values, "kind", row.Line);
            if (!ContractTokens.TryParse(kind, out ContractFieldKind parsed))
            {
                throw FlowTextException.AtKey(row.Line, "kind", "is not a field kind");
            }

            bool required = row.Values.TryGetValue("required", out (FlowValue Value, int Line) flag)
                ? flag.Value is FlowBooleanValue boolean
                    ? boolean.Value
                    : throw FlowTextException.AtKey(flag.Line, "required", "must be true or false")
                : throw FlowTextException.AtKey(row.Line, "required", "is missing");
            ulong? max = row.Values.ContainsKey("max_len")
                ? (ulong)Whole(row.Values, "max_len", row.Line, 0, long.MaxValue)
                : null;
            return new ContractField(Text(row.Values, "name", row.Line), parsed, required, max);
        }

        private static ContractTransition Transition(Row row)
        {
            Allow(row, ["by", "after", "next", "outcome", "bound"]);
            string? next = OptionalText(row.Values, "next");
            string? outcome = OptionalText(row.Values, "outcome");
            if ((next is null) == (outcome is null))
            {
                throw FlowTextException.At(row.Line, "an order rule has exactly one of `next` and `outcome`");
            }

            int? bound = row.Values.ContainsKey("bound")
                ? (int)Whole(row.Values, "bound", row.Line, 0, int.MaxValue)
                : null;
            return new ContractTransition(Text(row.Values, "by", row.Line), OptionalText(row.Values, "after"), next, outcome, bound);
        }

        private static ContractOutcome Outcome(Row row)
        {
            Allow(row, ["name", "kind"]);
            string kind = Text(row.Values, "kind", row.Line);
            if (!ContractTokens.TryParse(kind, out ContractOutcomeKind parsed))
            {
                throw FlowTextException.AtKey(row.Line, "kind", "is not an outcome kind");
            }

            return new ContractOutcome(Text(row.Values, "name", row.Line), parsed);
        }

        private static ContractTimeout Timeout(Row row)
        {
            Allow(row, ["message", "by", "seconds", "outcome"]);
            return new ContractTimeout(
                Text(row.Values, "message", row.Line),
                Text(row.Values, "by", row.Line),
                Whole(row.Values, "seconds", row.Line, 0, long.MaxValue),
                OptionalText(row.Values, "outcome"));
        }

        private static ContractSignature Signature(Row row)
        {
            Allow(row, ["role", "algorithm", "key_id", "signed_hash", "signature"]);
            return new ContractSignature(
                Text(row.Values, "role", row.Line),
                Text(row.Values, "algorithm", row.Line),
                Text(row.Values, "key_id", row.Line),
                Text(row.Values, "signed_hash", row.Line),
                Text(row.Values, "signature", row.Line));
        }

        private static void Allow(Row row, string[] allowed)
        {
            foreach (KeyValuePair<string, (FlowValue Value, int Line)> pair in row.Values)
            {
                if (Array.IndexOf(allowed, pair.Key) < 0)
                {
                    throw FlowTextException.AtUnknownKey(pair.Value.Line, allowed);
                }
            }
        }

        private static string Text(Dictionary<string, (FlowValue Value, int Line)> values, string key, int tableLine)
        {
            if (!values.TryGetValue(key, out (FlowValue Value, int Line) found))
            {
                throw FlowTextException.AtKey(tableLine, key, "is missing");
            }

            return found.Value is FlowStringValue text
                ? text.Value
                : throw FlowTextException.AtKey(found.Line, key, "must be a string");
        }

        private static string? OptionalText(Dictionary<string, (FlowValue Value, int Line)> values, string key)
        {
            if (!values.TryGetValue(key, out (FlowValue Value, int Line) found))
            {
                return null;
            }

            return found.Value is FlowStringValue text
                ? text.Value
                : throw FlowTextException.AtKey(found.Line, key, "must be a string");
        }

        private static long Whole(Dictionary<string, (FlowValue Value, int Line)> values, string key, int tableLine, long min, long max)
        {
            if (!values.TryGetValue(key, out (FlowValue Value, int Line) found))
            {
                throw FlowTextException.AtKey(tableLine, key, "is missing");
            }

            if (found.Value is not FlowIntegerValue number)
            {
                throw FlowTextException.AtKey(found.Line, key, "must be a whole number");
            }

            return number.Value >= min && number.Value <= max
                ? number.Value
                : throw FlowTextException.AtKey(found.Line, key, "is out of range");
        }

        private class Row(int line)
        {

            public int Line { get; } = line;

            public Dictionary<string, (FlowValue Value, int Line)> Values { get; } = new(StringComparer.Ordinal);
        }

        private sealed class MessageRow(int line) : Row(line)
        {

            public List<Row> Fields { get; } = [];
        }
    }
}
