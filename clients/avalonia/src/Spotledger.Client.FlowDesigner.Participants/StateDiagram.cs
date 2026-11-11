using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;

namespace Spotledger.Client.FlowDesigner.Participants;

/// What a state of the generated diagram stands for.
public enum StateKind
{
    Start,
    Message,
    Outcome,
}

/// One state of the conversation: the start, a message just exchanged, or an outcome. <see cref="Layer"/>
/// is the number of steps from the start and <see cref="Row"/> the place within the layer, so a
/// renderer needs no layout of its own.
public sealed record StateNodeInfo(string Id, StateKind Kind, string Label, string? Role, int Layer, int Row, bool Reachable);

/// One order rule as an arrow: the role that acts, the repetition bound, and the rule's index in
/// the contract's order table.
public sealed record StateEdgeInfo(string From, string To, string By, int? Bound, int RuleIndex);

/// The state diagram generated from a contract's order table (ADR-080 section 5.2): a state per
/// message and per outcome, an arrow per rule. The table is the source of truth and the diagram is
/// never edited, only regenerated, so the two cannot disagree.
public sealed class StateDiagram
{

    public const string StartId = "start";

    private StateDiagram(IReadOnlyList<StateNodeInfo> nodes, IReadOnlyList<StateEdgeInfo> edges)
    {
        Nodes = nodes;
        Edges = edges;
    }

    public IReadOnlyList<StateNodeInfo> Nodes { get; }

    public IReadOnlyList<StateEdgeInfo> Edges { get; }

    public int LayerCount => Nodes.Count == 0 ? 0 : Nodes.Max(node => node.Layer) + 1;

    /// The states no order rule reaches from the start: the designer flags them (the server's
    /// <c>message_unreachable</c> says the same in words).
    public IReadOnlyList<StateNodeInfo> Unreachable => [.. Nodes.Where(node => !node.Reachable)];

    public StateNodeInfo? Node(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        return Nodes.FirstOrDefault(node => string.Equals(node.Id, id, StringComparison.Ordinal));
    }

    public static string MessageId(string message) => "message:" + message;

    public static string OutcomeId(string outcome) => "outcome:" + outcome;

    public static StateDiagram From(ProcessContract contract)
    {
        ArgumentNullException.ThrowIfNull(contract);
        List<(string Id, StateKind Kind, string Label, string? Role)> states = [(StartId, StateKind.Start, "start", null)];
        foreach (ContractMessage message in contract.Messages)
        {
            states.Add((MessageId(message.Name), StateKind.Message, message.Name, message.From));
        }

        foreach (ContractOutcome outcome in contract.Outcomes)
        {
            states.Add((OutcomeId(outcome.Name), StateKind.Outcome, outcome.Name, null));
        }

        List<StateEdgeInfo> edges = [];
        for (int i = 0; i < contract.Order.Count; i++)
        {
            ContractTransition rule = contract.Order[i];
            string from = rule.After is null ? StartId : MessageId(rule.After);
            string to = rule.EndsConversation ? OutcomeId(rule.Target) : MessageId(rule.Target);
            if (states.Any(s => s.Id == from) && states.Any(s => s.Id == to))
            {
                edges.Add(new StateEdgeInfo(from, to, rule.By, rule.Bound, i));
            }
        }

        Dictionary<string, int> layer = new(StringComparer.Ordinal) { [StartId] = 0 };
        Queue<string> queue = new([StartId]);
        while (queue.Count > 0)
        {
            string current = queue.Dequeue();
            foreach (StateEdgeInfo edge in edges.Where(e => e.From == current))
            {
                if (layer.TryAdd(edge.To, layer[current] + 1))
                {
                    queue.Enqueue(edge.To);
                }
            }
        }

        int deepest = layer.Values.Max();
        Dictionary<int, int> rows = [];
        List<StateNodeInfo> nodes = [];
        foreach ((string id, StateKind kind, string label, string? role) in states)
        {
            bool reached = layer.TryGetValue(id, out int found);
            int at = reached ? found : deepest + 1;
            int row = rows.GetValueOrDefault(at);
            rows[at] = row + 1;
            nodes.Add(new StateNodeInfo(id, kind, label, role, at, row, reached));
        }

        return new StateDiagram(nodes, edges);
    }

    /// The diagram as text, one arrow per line: also what a screen reader is given.
    public string ToText()
    {
        StringBuilder builder = new();
        foreach (StateEdgeInfo edge in Edges)
        {
            builder.Append(Node(edge.From)?.Label).Append(" -> ").Append(Node(edge.To)?.Label);
            builder.Append(" [").Append(edge.By);
            if (edge.Bound is { } bound)
            {
                builder.Append(", at most ").Append(bound.ToString(CultureInfo.InvariantCulture));
            }

            builder.Append("]\n");
        }

        foreach (StateNodeInfo node in Unreachable)
        {
            builder.Append(node.Label).Append(" is not reached\n");
        }

        return builder.ToString();
    }
}
