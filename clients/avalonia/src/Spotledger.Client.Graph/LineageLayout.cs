namespace Spotledger.Client.Graph;

/// <summary>
/// The deterministic layered left-to-right layout of a lineage graph. Layers come from the
/// longest path over the graph's strongly-connected components — contracting SCCs first is
/// what makes a cyclic input (a returned-goods loop, say) terminate with a stable answer
/// instead of chasing itself. Within a layer nodes keep the snapshot's order, so the same
/// snapshot always produces the same coordinates. A graph over 200 nodes is computed on a
/// background thread (<see cref="ComputeAsync"/>).
/// </summary>
public static class LineageLayoutEngine
{
    /// <summary>The card width in layout space.</summary>
    public const double CardWidth = 200;

    /// <summary>The card height in layout space.</summary>
    public const double CardHeight = 64;

    /// <summary>The gap between layers (columns).</summary>
    public const double LayerGap = 80;

    /// <summary>The gap between cards in a column.</summary>
    public const double RowGap = 24;

    /// <summary>The padding around the whole layout.</summary>
    public const double Padding = 24;

    /// <summary>The node count above which a layout is computed off the calling thread.</summary>
    public const int OffThreadThreshold = 200;

    /// <summary>Computes the layout synchronously. Pure: same snapshot, same coordinates.</summary>
    /// <param name="snapshot">The graph slice to lay out.</param>
    public static LineageLayout Compute(GraphSnapshot snapshot)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        var nodes = snapshot.Nodes;
        if (nodes.Count == 0)
        {
            return LineageLayout.Empty with { ComputedOnThreadId = Environment.CurrentManagedThreadId };
        }

        var index = BuildIndex(nodes);
        var edges = new List<LineageEdge>(snapshot.Edges.Count);
        foreach (var edge in snapshot.Edges)
        {
            if (index.ContainsKey(edge.From) && index.ContainsKey(edge.To))
            {
                edges.Add(edge);
            }
        }

        var (outgoing, incoming) = BuildAdjacency(nodes.Count, edges, index);
        var component = StronglyConnected(nodes.Count, outgoing, incoming);
        var componentCount = 0;
        foreach (var id in component)
        {
            componentCount = Math.Max(componentCount, id + 1);
        }

        var layer = LayerComponents(component, componentCount, edges, index);
        var placements = Place(nodes, component, layer);
        var routes = RouteEdges(edges, placements);
        var layerCount = layer.Length == 0 ? 0 : layer.Max() + 1;
        var rowCount = 0;
        foreach (var placement in placements)
        {
            rowCount = Math.Max(rowCount, placement.Row + 1);
        }

        var width = (Padding * 2) + (layerCount * CardWidth) + (Math.Max(0, layerCount - 1) * LayerGap);
        var height = (Padding * 2) + (rowCount * CardHeight) + (Math.Max(0, rowCount - 1) * RowGap);
        return new LineageLayout(placements, routes, layerCount, width, height, Environment.CurrentManagedThreadId);
    }

    /// <summary>Computes the layout, off the calling thread for a graph over
    /// <see cref="OffThreadThreshold"/> nodes so the UI thread never runs a big layout.</summary>
    /// <param name="snapshot">The graph slice.</param>
    /// <param name="cancellationToken">Cancels the computation.</param>
    public static Task<LineageLayout> ComputeAsync(GraphSnapshot snapshot, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        return snapshot.Nodes.Count <= OffThreadThreshold
            ? Task.FromResult(Compute(snapshot))
            : Task.Run(() => Compute(snapshot), cancellationToken);
    }

    private static Dictionary<GraphNodeKey, int> BuildIndex(IReadOnlyList<LineageNode> nodes)
    {
        var index = new Dictionary<GraphNodeKey, int>(nodes.Count);
        for (var i = 0; i < nodes.Count; i++)
        {
            index.TryAdd(nodes[i].Key, i);
        }

        return index;
    }

    private static (List<int>[] Outgoing, List<int>[] Incoming) BuildAdjacency(
        int count,
        List<LineageEdge> edges,
        Dictionary<GraphNodeKey, int> index)
    {
        var outgoing = new List<int>[count];
        var incoming = new List<int>[count];
        for (var i = 0; i < count; i++)
        {
            outgoing[i] = [];
            incoming[i] = [];
        }

        foreach (var edge in edges)
        {
            var from = index[edge.From];
            var to = index[edge.To];
            outgoing[from].Add(to);
            incoming[to].Add(from);
        }

        return (outgoing, incoming);
    }

    private static int[] StronglyConnected(int count, List<int>[] outgoing, List<int>[] incoming)
    {
        var order = new List<int>(count);
        var seen = new bool[count];
        for (var start = 0; start < count; start++)
        {
            if (seen[start])
            {
                continue;
            }

            var stack = new Stack<(int Node, int Next)>();
            stack.Push((start, 0));
            seen[start] = true;
            while (stack.Count > 0)
            {
                var (node, next) = stack.Pop();
                if (next < outgoing[node].Count)
                {
                    stack.Push((node, next + 1));
                    var child = outgoing[node][next];
                    if (!seen[child])
                    {
                        seen[child] = true;
                        stack.Push((child, 0));
                    }
                }
                else
                {
                    order.Add(node);
                }
            }
        }

        var component = new int[count];
        Array.Fill(component, -1);
        var nextComponent = 0;
        for (var i = order.Count - 1; i >= 0; i--)
        {
            var start = order[i];
            if (component[start] >= 0)
            {
                continue;
            }

            var pending = new Stack<int>();
            pending.Push(start);
            component[start] = nextComponent;
            while (pending.Count > 0)
            {
                var node = pending.Pop();
                foreach (var child in incoming[node])
                {
                    if (component[child] < 0)
                    {
                        component[child] = nextComponent;
                        pending.Push(child);
                    }
                }
            }

            nextComponent++;
        }

        return component;
    }

    private static int[] LayerComponents(
        int[] component,
        int componentCount,
        List<LineageEdge> edges,
        Dictionary<GraphNodeKey, int> index)
    {
        var layer = new int[componentCount];
        var indegree = new int[componentCount];
        var successors = new List<int>[componentCount];
        for (var i = 0; i < componentCount; i++)
        {
            successors[i] = [];
        }

        var seenEdges = new HashSet<(int From, int To)>();
        foreach (var edge in edges)
        {
            var from = component[index[edge.From]];
            var to = component[index[edge.To]];
            if (from == to || !seenEdges.Add((from, to)))
            {
                continue;
            }

            successors[from].Add(to);
            indegree[to]++;
        }

        var ready = new SortedSet<int>();
        for (var i = 0; i < componentCount; i++)
        {
            if (indegree[i] == 0)
            {
                ready.Add(i);
            }
        }

        while (ready.Count > 0)
        {
            var current = ready.Min;
            ready.Remove(current);
            foreach (var to in successors[current])
            {
                layer[to] = Math.Max(layer[to], layer[current] + 1);
                if (--indegree[to] == 0)
                {
                    ready.Add(to);
                }
            }
        }

        // A condensation is a DAG by construction, so every component is processed above. If a
        // malformed edge set ever left a component unprocessed, it keeps layer 0 instead of the
        // layout looping.
        return layer;
    }

    private static List<LineageNodePlacement> Place(IReadOnlyList<LineageNode> nodes, int[] component, int[] layer)
    {
        var layerCount = layer.Length == 0 ? 0 : layer.Max() + 1;
        var assignments = new int[nodes.Count];
        var rowsPerLayer = new int[layerCount];
        for (var i = 0; i < nodes.Count; i++)
        {
            assignments[i] = layer[component[i]];
            rowsPerLayer[assignments[i]]++;
        }

        var maxRows = 0;
        foreach (var rows in rowsPerLayer)
        {
            maxRows = Math.Max(maxRows, rows);
        }

        var starts = new double[layerCount];
        for (var l = 0; l < layerCount; l++)
        {
            starts[l] = Padding + ((maxRows - rowsPerLayer[l]) * (CardHeight + RowGap) / 2);
        }

        var nextRow = new int[layerCount];
        var placements = new List<LineageNodePlacement>(nodes.Count);
        for (var i = 0; i < nodes.Count; i++)
        {
            var l = assignments[i];
            var row = nextRow[l]++;
            placements.Add(new LineageNodePlacement(
                nodes[i],
                l,
                row,
                new LineagePoint(
                    Padding + (l * (CardWidth + LayerGap)),
                    starts[l] + (row * (CardHeight + RowGap)))));
        }

        return placements;
    }

    private static List<LineageEdgeRoute> RouteEdges(List<LineageEdge> edges, List<LineageNodePlacement> placements)
    {
        var byKey = new Dictionary<GraphNodeKey, LineageNodePlacement>(placements.Count);
        foreach (var placement in placements)
        {
            byKey.TryAdd(placement.Key, placement);
        }

        var routes = new List<LineageEdgeRoute>(edges.Count);
        foreach (var edge in edges)
        {
            if (!byKey.TryGetValue(edge.From, out var from) || !byKey.TryGetValue(edge.To, out var to))
            {
                continue;
            }

            var fromY = from.Position.Y + (CardHeight / 2);
            var toY = to.Position.Y + (CardHeight / 2);
            List<LineagePoint> points;
            if (to.Layer > from.Layer)
            {
                var start = new LineagePoint(from.Position.X + CardWidth, fromY);
                var end = new LineagePoint(to.Position.X, toY);
                var mid = (start.X + end.X) / 2;
                points = [start, new LineagePoint(mid, start.Y), new LineagePoint(mid, end.Y), end];
            }
            else
            {
                // A backward or same-layer edge routes below both cards on a fixed track, so a
                // cycle still has a stable, non-overlapping path.
                var bottom = Math.Max(from.Position.Y, to.Position.Y) + CardHeight + (RowGap / 2);
                var start = new LineagePoint(from.Position.X + (CardWidth / 2), from.Position.Y + CardHeight);
                var end = new LineagePoint(to.Position.X + (CardWidth / 2), to.Position.Y + CardHeight);
                points =
                [
                    start,
                    new LineagePoint(start.X, bottom),
                    new LineagePoint(end.X, bottom),
                    end,
                ];
            }

            routes.Add(new LineageEdgeRoute(edge.From, edge.To, edge.Label, points));
        }

        return routes;
    }
}
