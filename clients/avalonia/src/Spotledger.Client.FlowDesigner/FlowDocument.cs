using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;

namespace Spotledger.Client.FlowDesigner;

/// How many incoming edges must be taken before a node may start.
public enum FlowJoinKind
{
    All,
    Any,
    Quorum,
}

/// The lifecycle status of a process version, in the text form's words.
public enum FlowVersionStatus
{
    Draft,
    Published,
    Retired,
}

/// A point in designer space (grid units, never money or time: ADR-12).
public readonly record struct FlowPoint(double X, double Y)
{

    public FlowPoint Offset(double dx, double dy) => new(X + dx, Y + dy);

    public override string ToString() =>
        X.ToString("0.###", CultureInfo.InvariantCulture) + ","
        + Y.ToString("0.###", CultureInfo.InvariantCulture);
}

/// The identity of an edge in the document: its two ends.
public readonly record struct FlowEdgeKey(string From, string To);

/// One node of the process graph: its typed kind, its per-kind config, its join rule, its error
/// branch and retry policy, and — designer-only, never in the text form — where the canvas draws
/// it.
public sealed class FlowNode
{

    internal FlowNode(string id, string kind)
    {
        Id = id;
        Kind = kind;
    }

    public string Id { get; internal set; }

    public string Kind { get; internal set; }

    public IReadOnlyDictionary<string, FlowValue> Config => Map;

    public FlowJoinKind Join { get; internal set; }

    public int JoinCount { get; internal set; }

    public bool Terminal { get; internal set; }

    public string? Error { get; internal set; }

    public ulong? TimeoutSecs { get; internal set; }

    public int RetryAttempts { get; internal set; } = 1;

    public ulong RetryDelaySecs { get; internal set; }

    public FlowPoint Position { get; internal set; }

    internal Dictionary<string, FlowValue> Map { get; } = new(StringComparer.Ordinal);

    public FlowNode Clone()
    {
        FlowNode copy = new(Id, Kind)
        {
            Join = Join,
            JoinCount = JoinCount,
            Terminal = Terminal,
            Error = Error,
            TimeoutSecs = TimeoutSecs,
            RetryAttempts = RetryAttempts,
            RetryDelaySecs = RetryDelaySecs,
            Position = Position,
        };
        foreach (KeyValuePair<string, FlowValue> pair in Map)
        {
            copy.Map[pair.Key] = pair.Value;
        }

        return copy;
    }

    public FlowValue? ConfigValue(string key)
    {
        ArgumentNullException.ThrowIfNull(key);
        return Map.TryGetValue(key, out FlowValue? value) ? value : null;
    }

    internal void ReplaceConfig(IReadOnlyDictionary<string, FlowValue> values)
    {
        Map.Clear();
        foreach (KeyValuePair<string, FlowValue> pair in values)
        {
            Map[pair.Key] = pair.Value;
        }
    }
}

/// One edge: a connection from one node to another, with an optional condition.
public sealed class FlowEdge
{

    internal FlowEdge(string from, string to)
    {
        From = from;
        To = to;
    }

    public string From { get; internal set; }

    public string To { get; internal set; }

    public string? Condition { get; internal set; }

    public string? Rule { get; internal set; }

    public string? Label { get; internal set; }

    public bool Default { get; internal set; }

    public FlowEdgeKey Key => new(From, To);

    public FlowEdge Clone() =>
        new(From, To) { Condition = Condition, Rule = Rule, Label = Label, Default = Default };
}

/// A named boolean expression an edge may reference.
public sealed class FlowRule
{

    internal FlowRule(string id, string expression)
    {
        Id = id;
        Expression = expression;
    }

    public string Id { get; internal set; }

    public string Expression { get; internal set; }

    public FlowRule Clone() => new(Id, Expression);
}

/// The process document in memory: one process version, exactly the content of the process text
/// form, plus the designer's layout. Nodes, edges and rules keep definition order; config keys
/// render sorted. Nothing here talks to a server: the editing commands
/// (<see cref="FlowEditor"/>) and <see cref="IFlowDesignerServer"/> do.
public sealed class FlowDocument
{

    private readonly List<FlowNode> _nodes = [];

    private readonly List<FlowEdge> _edges = [];

    private readonly List<FlowRule> _rules = [];

    public FlowDocument(string name, string app)
    {
        Name = FlowId.Require(name, nameof(name));
        App = FlowId.Require(app, nameof(app));
    }

    public string Name { get; internal set; }

    public string App { get; internal set; }

    public string? Title { get; internal set; }

    public int Version { get; internal set; } = 1;

    public FlowVersionStatus Status { get; internal set; } = FlowVersionStatus.Draft;

    public IReadOnlyList<FlowNode> Nodes => _nodes;

    public IReadOnlyList<FlowEdge> Edges => _edges;

    public IReadOnlyList<FlowRule> Rules => _rules;

    public FlowNode? Node(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        foreach (FlowNode node in _nodes)
        {
            if (string.Equals(node.Id, id, StringComparison.Ordinal))
            {
                return node;
            }
        }

        return null;
    }

    public FlowRule? Rule(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        foreach (FlowRule rule in _rules)
        {
            if (string.Equals(rule.Id, id, StringComparison.Ordinal))
            {
                return rule;
            }
        }

        return null;
    }

    public IReadOnlyList<FlowEdge> Outgoing(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        return _edges.Where(edge => string.Equals(edge.From, id, StringComparison.Ordinal)).ToList();
    }

    public IReadOnlyList<FlowEdge> Incoming(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        return _edges.Where(edge => string.Equals(edge.To, id, StringComparison.Ordinal)).ToList();
    }

    public string ToText() => ProcessTextWriter.Write(this);

    public static FlowDocument FromText(string text) => ProcessTextReader.Read(text);

    public string NextFreeId(string baseId)
    {
        ArgumentNullException.ThrowIfNull(baseId);
        string root = FlowId.Sanitize(baseId);
        if (Node(root) is null)
        {
            return root;
        }

        for (int suffix = 2; suffix < 10_000; suffix++)
        {
            string candidate = root + "_" + suffix.ToString(CultureInfo.InvariantCulture);
            if (Node(candidate) is null && FlowId.IsName(candidate))
            {
                return candidate;
            }
        }

        throw new InvalidOperationException("No free node id is left for " + root + ".");
    }

    internal void AddNode(FlowNode node) => InsertNode(_nodes.Count, node);

    internal void InsertNode(int index, FlowNode node)
    {
        _nodes.Insert(Math.Clamp(index, 0, _nodes.Count), node);
    }

    internal void RemoveNodeAt(int index) => _nodes.RemoveAt(index);

    internal int IndexOfNode(string id)
    {
        for (int i = 0; i < _nodes.Count; i++)
        {
            if (string.Equals(_nodes[i].Id, id, StringComparison.Ordinal))
            {
                return i;
            }
        }

        return -1;
    }

    internal void AddEdge(FlowEdge edge) => _edges.Add(edge);

    internal void InsertEdge(int index, FlowEdge edge) =>
        _edges.Insert(Math.Clamp(index, 0, _edges.Count), edge);

    internal void RemoveEdge(FlowEdge edge) => _edges.Remove(edge);

    internal void AddRule(FlowRule rule) => _rules.Add(rule);

    internal void RemoveRule(FlowRule rule) => _rules.Remove(rule);

    internal int NextVersion() => Version + 1;

    public override string ToString()
    {
        StringBuilder builder = new();
        builder.Append(Name).Append('@').Append(Version.ToString(CultureInfo.InvariantCulture));
        builder.Append('[').Append(_nodes.Count.ToString(CultureInfo.InvariantCulture)).Append(" nodes, ");
        builder.Append(_edges.Count.ToString(CultureInfo.InvariantCulture)).Append(" edges]");
        return builder.ToString();
    }
}
