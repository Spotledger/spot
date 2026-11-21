using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner;

/// What the person has selected: nodes and edges, in the order they were picked. A selection is
/// plain data (no Avalonia types), so the copy/paste, keyboard and render paths can all agree on it
/// and a test can drive it directly.
public sealed class FlowSelection
{

    private readonly List<string> _nodes = [];

    private readonly List<FlowEdgeKey> _edges = [];

    public IReadOnlyList<string> NodeIds => _nodes;

    public IReadOnlyList<FlowEdgeKey> EdgeKeys => _edges;

    public bool IsEmpty => _nodes.Count == 0 && _edges.Count == 0;

    public int Count => _nodes.Count + _edges.Count;

    public string? SingleNodeId => _nodes.Count == 1 && _edges.Count == 0 ? _nodes[0] : null;

    public bool Contains(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        return _nodes.Contains(id, StringComparer.Ordinal);
    }

    public bool ContainsEdge(FlowEdgeKey key) => _edges.Contains(key);

    public void Clear()
    {
        _nodes.Clear();
        _edges.Clear();
    }

    public void SelectOnly(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        Clear();
        _nodes.Add(id);
    }

    public void SelectOnly(IEnumerable<string> ids)
    {
        ArgumentNullException.ThrowIfNull(ids);
        Clear();
        foreach (string id in ids)
        {
            if (!_nodes.Contains(id, StringComparer.Ordinal))
            {
                _nodes.Add(id);
            }
        }
    }

    public void Add(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        if (!_nodes.Contains(id, StringComparer.Ordinal))
        {
            _nodes.Add(id);
        }
    }

    public bool Toggle(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        if (_nodes.Remove(id))
        {
            return false;
        }

        _nodes.Add(id);
        return true;
    }

    public bool ToggleEdge(FlowEdgeKey key)
    {
        if (_edges.Remove(key))
        {
            return false;
        }

        _edges.Add(key);
        return true;
    }

    public void Prune(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        _nodes.RemoveAll(id => document.Node(id) is null);
        _edges.RemoveAll(key => !document.Edges.Any(edge => edge.Key == key));
    }

    public string Describe() => FlowStrings.Selected(_nodes.Count);

    public override string ToString() =>
        string.Join(',', _nodes) + "|" + string.Join(',', _edges.Select(key => key.From + "->" + key.To));
}

/// The designer's clipboard: the copied nodes with the edges between them. A paste renames any node
/// whose id is already taken (<c>start</c> becomes <c>start_2</c>), rewires the copied edges to the
/// new ids, and lands slightly offset, snapped to the grid — as one undoable command.
public sealed class FlowClipboard
{

    private readonly List<FlowNode> _nodes = [];

    private readonly List<FlowEdge> _edges = [];

    public bool IsEmpty => _nodes.Count == 0;

    public int NodeCount => _nodes.Count;

    public int EdgeCount => _edges.Count;

    public void Copy(FlowDocument document, FlowSelection selection)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(selection);
        _nodes.Clear();
        _edges.Clear();
        foreach (string id in selection.NodeIds)
        {
            FlowNode? node = document.Node(id);
            if (node is not null)
            {
                _nodes.Add(node.Clone());
            }
        }

        HashSet<string> copied = new(_nodes.Select(node => node.Id), StringComparer.Ordinal);
        foreach (FlowEdge edge in document.Edges)
        {
            if (copied.Contains(edge.From) && copied.Contains(edge.To))
            {
                _edges.Add(edge.Clone());
            }
        }
    }

    public PasteCommand? PreparePaste(FlowDocument document, double offset = FlowLayoutEngine.GridSize * 2)
    {
        ArgumentNullException.ThrowIfNull(document);
        if (_nodes.Count == 0)
        {
            return null;
        }

        return new PasteCommand(document, _nodes, _edges, offset);
    }
}

/// Pastes a copied set of nodes and edges, renaming ids that collide.
public sealed class PasteCommand : IFlowCommand
{

    private readonly List<FlowNode> _nodes = [];

    private readonly List<FlowEdge> _edges = [];

    internal PasteCommand(
        FlowDocument document,
        IEnumerable<FlowNode> nodes,
        IEnumerable<FlowEdge> edges,
        double offset)
    {
        ArgumentNullException.ThrowIfNull(document);
        Dictionary<string, string> renamed = new(StringComparer.Ordinal);
        foreach (FlowNode source in nodes)
        {
            string id = document.NextFreeId(source.Id);
            renamed[source.Id] = id;
            FlowNode copy = source.Clone();
            copy.Id = id;
            copy.Position = FlowLayoutEngine.Snap(copy.Position.Offset(offset, offset));
            _nodes.Add(copy);
        }

        foreach (FlowEdge source in edges)
        {
            if (renamed.TryGetValue(source.From, out string? from) && renamed.TryGetValue(source.To, out string? to))
            {
                FlowEdge copy = source.Clone();
                copy.From = from;
                copy.To = to;
                _edges.Add(copy);
            }
        }
    }

    public string Label => "Paste " + FlowStrings.Selected(_nodes.Count);

    public int NodeCount => _nodes.Count;

    public IReadOnlyList<string> NodeIds => [.. _nodes.Select(node => node.Id)];

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach (FlowNode node in _nodes)
        {
            document.AddNode(node.Clone());
        }

        foreach (FlowEdge edge in _edges)
        {
            document.AddEdge(edge.Clone());
        }
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach (FlowEdge edge in _edges)
        {
            FlowEdge? existing = document.Edges.FirstOrDefault(candidate => candidate.Key == edge.Key);
            if (existing is not null)
            {
                document.RemoveEdge(existing);
            }
        }

        foreach (FlowNode node in _nodes)
        {
            int index = document.IndexOfNode(node.Id);
            if (index >= 0)
            {
                document.RemoveNodeAt(index);
            }
        }
    }
}
