using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;

namespace Spotledger.Client.FlowDesigner;

/// One node's place in a computed layout.
public sealed record FlowLayoutNode(string Id, int Layer, FlowPoint Position);

/// The result of a layout pass.
public sealed record FlowLayoutResult(
    IReadOnlyList<FlowLayoutNode> Nodes,
    IReadOnlyList<string> CyclicNodes,
    double Width,
    double Height)
{

    public FlowLayoutNode? Node(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        return Nodes.FirstOrDefault(node => string.Equals(node.Id, id, StringComparison.Ordinal));
    }
}

/// The deterministic layered layout: a longest-path layering left to right, one column per layer,
/// the nodes of a column ordered by their definition order. Nothing depends on the clock, on
/// hashing or on iteration order, so the same document always lays out to the same points — which
/// is what makes the golden test worth having. A cycle (which the server reports as a problem)
/// still lays out: its nodes go into one extra column instead of looping the algorithm.
public static class FlowLayoutEngine
{

    public const double GridSize = 16;

    public const double NodeWidth = 176;

    public const double NodeHeight = 80;

    public const double ColumnGap = 96;

    public const double RowGap = 32;

    public const double Origin = 48;

    public static FlowLayoutResult Compute(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        IReadOnlyList<FlowNode> nodes = document.Nodes;
        Dictionary<string, int> index = new(StringComparer.Ordinal);
        for (int i = 0; i < nodes.Count; i++)
        {
            index[nodes[i].Id] = i;
        }

        // Longest path layering over the edges whose ends both exist; a node with no incoming
        // edge is a start. Repeated passes converge in at most |nodes| rounds.
        int[] layer = new int[nodes.Count];
        bool changed = true;
        for (int round = 0; round < nodes.Count && changed; round++)
        {
            changed = false;
            foreach (FlowEdge edge in document.Edges)
            {
                if (!index.TryGetValue(edge.From, out int from) || !index.TryGetValue(edge.To, out int to))
                {
                    continue;
                }

                if (from == to)
                {
                    continue;
                }

                int candidate = layer[from] + 1;
                if (candidate > layer[to] && candidate < nodes.Count + 1)
                {
                    layer[to] = candidate;
                    changed = true;
                }
            }
        }

        List<string> cyclic = [];
        int highest = layer.Length == 0 ? 0 : layer.Max();
        for (int i = 0; i < nodes.Count; i++)
        {
            if (layer[i] >= nodes.Count)
            {
                layer[i] = highest + 1;
                cyclic.Add(nodes[i].Id);
            }
        }

        Dictionary<int, List<int>> columns = [];
        for (int i = 0; i < nodes.Count; i++)
        {
            if (!columns.TryGetValue(layer[i], out List<int>? column))
            {
                column = [];
                columns[layer[i]] = column;
            }

            column.Add(i);
        }

        List<FlowLayoutNode> placements = new(nodes.Count);
        double width = Origin;
        double height = Origin;
        foreach (int column in columns.Keys.OrderBy(value => value))
        {
            List<int> rows = columns[column];
            double x = Origin + (column * (NodeWidth + ColumnGap));
            for (int row = 0; row < rows.Count; row++)
            {
                double y = Origin + (row * (NodeHeight + RowGap));
                FlowNode node = nodes[rows[row]];
                placements.Add(new FlowLayoutNode(node.Id, column, new FlowPoint(x, y)));
                width = Math.Max(width, x + NodeWidth + Origin);
                height = Math.Max(height, y + NodeHeight + Origin);
            }
        }

        cyclic.Sort(StringComparer.Ordinal);
        return new FlowLayoutResult(placements, cyclic, width, height);
    }

    public static void Apply(FlowDocument document, FlowLayoutResult result)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(result);
        foreach (FlowLayoutNode placement in result.Nodes)
        {
            FlowNode? node = document.Node(placement.Id);
            if (node is not null)
            {
                node.Position = placement.Position;
            }
        }
    }

    public static double Snap(double value) => Math.Round(value / GridSize, MidpointRounding.AwayFromZero) * GridSize;

    public static FlowPoint Snap(FlowPoint point) => new(Snap(point.X), Snap(point.Y));

    public static string Describe(FlowLayoutResult result)
    {
        ArgumentNullException.ThrowIfNull(result);
        IEnumerable<string> lines = result.Nodes
            .OrderBy(node => node.Id, StringComparer.Ordinal)
            .Select(node => string.Concat(
                node.Id,
                " ",
                node.Layer.ToString(CultureInfo.InvariantCulture),
                " ",
                node.Position.ToString()));
        return string.Join('\n', lines) + "\ncyclic: " + string.Join(',', result.CyclicNodes) + "\n";
    }
}

/// Applies a fresh auto-layout to the whole document as one undoable edit.
public sealed class AutoLayoutCommand : IFlowCommand
{

    private readonly Dictionary<string, FlowPoint> _before = new(StringComparer.Ordinal);

    private FlowLayoutResult? _result;

    public AutoLayoutCommand(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach (FlowNode node in document.Nodes)
        {
            _before[node.Id] = node.Position;
        }
    }

    public string Label => FlowStrings.AutoLayout;

    public FlowLayoutResult? Result => _result;

    public void Apply(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        _result = FlowLayoutEngine.Compute(document);
        FlowLayoutEngine.Apply(document, _result);
    }

    public void Undo(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        foreach (KeyValuePair<string, FlowPoint> pair in _before)
        {
            FlowNode? node = document.Node(pair.Key);
            if (node is not null)
            {
                node.Position = pair.Value;
            }
        }
    }
}
