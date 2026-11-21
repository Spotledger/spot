using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.FlowDesigner.Participants;

/// One row of the field mapping: a contract field and the expression that builds its value from the
/// request. Diagnostics come from the Expressions parser, the same one the evaluator uses, so the
/// editor and the engine agree on what an expression is.
public sealed record MessageMappingRow(
    ContractField Field,
    string Expression,
    ContractFieldClass Class,
    IReadOnlyList<FlowDiagnostic> Diagnostics,
    IReadOnlyList<string> Reads)
{

    public bool IsMapped => Expression.Length > 0;

    public bool IsSecret => Class == ContractFieldClass.Secret;

    public bool IsOk => Diagnostics.Count == 0 && !(Field.Required && !IsMapped) && !(IsSecret && IsMapped);
}

/// The field mapping of one send node: for each field of the message the contract lists, the
/// expression over the request that fills it. Nothing outside the field list can be mapped
/// (ADR-058 rule 6), and a field the metadata classifies as secret cannot be mapped at all.
///
/// The server's send config has one <c>mapping</c> key (a mapping name or an expression), so the
/// field map is written into that key as one <c>field = expression</c> line per field; a value
/// without such a line is a mapping name and stays untouched. This is recorded under the open
/// questions of the task.
public sealed class MessageMapping
{

    private readonly ContractMessage _message;

    private readonly IContractFieldClassifier _classifier;

    private readonly Dictionary<string, string> _expressions = new(StringComparer.Ordinal);

    public MessageMapping(ContractMessage message, string? existing = null, IContractFieldClassifier? classifier = null)
    {
        ArgumentNullException.ThrowIfNull(message);
        _message = message;
        _classifier = classifier ?? PublicFields.Instance;
        if (existing is not null && TryParseLines(existing, out Dictionary<string, string> parsed))
        {
            foreach (KeyValuePair<string, string> pair in parsed)
            {
                if (message.Field(pair.Key) is not null)
                {
                    _expressions[pair.Key] = pair.Value;
                }
            }
        }
    }

    public ContractMessage Message => _message;

    /// One row per field of the message, in the contract's order.
    public IReadOnlyList<MessageMappingRow> Rows => [.. _message.Fields.Select(Row)];

    /// Whether the text is a field map (as opposed to a mapping name or a single expression).
    public static bool IsFieldMap(string? text) => text is not null && TryParseLines(text, out _);

    /// Sets the expression of one field. A field the contract does not list is refused.
    public bool Set(string field, string expression, out string? error)
    {
        ArgumentNullException.ThrowIfNull(field);
        ArgumentNullException.ThrowIfNull(expression);
        error = null;
        if (_message.Field(field) is null)
        {
            error = ParticipantStrings.FieldNotInContract;
            return false;
        }

        string trimmed = expression.Trim();
        if (trimmed.Contains('\n', StringComparison.Ordinal))
        {
            error = ParticipantStrings.MappingOneLine;
            return false;
        }

        if (trimmed.Length == 0)
        {
            _expressions.Remove(field);
        }
        else
        {
            _expressions[field] = trimmed;
        }

        return true;
    }

    /// The problems that stop the mapping from being sent: a required field with no expression, a
    /// secret field mapped, an expression that does not parse.
    public IReadOnlyList<string> Problems()
    {
        List<string> problems = [];
        foreach (MessageMappingRow row in Rows)
        {
            if (row.IsSecret && row.IsMapped)
            {
                problems.Add(ParticipantStrings.SecretFieldMapped(row.Field.Name));
            }

            if (row.Field.Required && !row.IsMapped && !row.IsSecret)
            {
                problems.Add(ParticipantStrings.RequiredFieldUnmapped(row.Field.Name));
            }

            foreach (FlowDiagnostic diagnostic in row.Diagnostics)
            {
                problems.Add(ParticipantStrings.BadExpression(row.Field.Name, diagnostic.Message));
            }
        }

        return problems;
    }

    /// The text for the node's <c>mapping</c> key, or an empty string when nothing is mapped.
    public string ToMappingText()
    {
        StringBuilder builder = new();
        foreach (ContractField field in _message.Fields)
        {
            if (_expressions.TryGetValue(field.Name, out string? expression))
            {
                if (builder.Length > 0)
                {
                    builder.Append('\n');
                }

                builder.Append(field.Name).Append(" = ").Append(expression);
            }
        }

        return builder.ToString();
    }

    /// The command that writes the mapping into the node, or null when there is nothing to write.
    public IFlowCommand? ToCommand(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        string text = ToMappingText();
        return text.Length == 0
            ? new SetConfigCommand(nodeId, ParticipantKinds.MappingKey, null)
            : new SetConfigCommand(nodeId, ParticipantKinds.MappingKey, FlowValue.Text(text));
    }

    private MessageMappingRow Row(ContractField field)
    {
        string expression = _expressions.TryGetValue(field.Name, out string? found) ? found : string.Empty;
        return new MessageMappingRow(
            field,
            expression,
            _classifier.Classify(field.Name),
            FlowExpressionEditor.Diagnose(expression),
            ReadsOf(expression));
    }

    /// The request fields an expression reads, as <c>doc.total</c> style paths, in first-seen order.
    public static IReadOnlyList<string> ReadsOf(string? expression)
    {
        if (string.IsNullOrWhiteSpace(expression))
        {
            return [];
        }

        ExpressionNode tree;
        try
        {
            tree = ExpressionParser.Parse(expression);
        }
        catch (ExpressionParseException)
        {
            return [];
        }

        List<string> reads = [];
        Collect(tree, reads);
        return reads;
    }

    private static void Collect(ExpressionNode node, List<string> reads)
    {
        switch (node)
        {
            case PathNode path:
                string text = (path.Root == ExpressionRoot.Doc ? "doc" : "parent") + "." + string.Join('.', path.Names);
                if (!reads.Contains(text, StringComparer.Ordinal))
                {
                    reads.Add(text);
                }

                break;
            case UnaryNode unary:
                Collect(unary.Inner, reads);
                break;
            case BinaryNode binary:
                Collect(binary.Left, reads);
                Collect(binary.Right, reads);
                break;
            case TernaryNode ternary:
                Collect(ternary.Condition, reads);
                Collect(ternary.WhenTrue, reads);
                Collect(ternary.WhenFalse, reads);
                break;
            case CallNode call:
                foreach (ExpressionNode argument in call.Arguments)
                {
                    Collect(argument, reads);
                }

                break;
            case ListNode list:
                foreach (ExpressionNode item in list.Items)
                {
                    Collect(item, reads);
                }

                break;
            default:
                break;
        }
    }

    private static bool TryParseLines(string text, out Dictionary<string, string> parsed)
    {
        parsed = new Dictionary<string, string>(StringComparer.Ordinal);
        bool any = false;
        foreach (string raw in text.Replace("\r\n", "\n", StringComparison.Ordinal).Split('\n'))
        {
            string line = raw.Trim();
            if (line.Length == 0)
            {
                continue;
            }

            int equals = line.IndexOf('=', StringComparison.Ordinal);
            bool isPair = equals > 0
                && (equals + 1 >= line.Length || line[equals + 1] != '=')
                && IsFieldName(line[..equals].Trim());
            if (!isPair)
            {
                parsed.Clear();
                return false;
            }

            parsed[line[..equals].Trim()] = line[(equals + 1)..].Trim();
            any = true;
        }

        return any;
    }

    /// A contract field name: dotted lower-case segments.
    internal static bool IsFieldName(string name)
    {
        if (name.Length == 0 || name.Length > 128)
        {
            return false;
        }

        foreach (string segment in name.Split('.'))
        {
            if (segment.Length == 0 || segment[0] < 'a' || segment[0] > 'z')
            {
                return false;
            }

            foreach (char c in segment)
            {
                bool ok = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
                if (!ok)
                {
                    return false;
                }
            }
        }

        return true;
    }
}
