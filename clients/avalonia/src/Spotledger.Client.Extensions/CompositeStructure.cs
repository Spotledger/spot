namespace Spotledger.Client.Extensions;

/// <summary>
/// The component reference graph: which components a body uses, which sit on a cycle, and
/// how large an expansion is. The arithmetic mirrors the server's <c>spot-ui-ext</c>
/// (<c>composite.rs</c>) so a document the server accepted measures the same here: a
/// layout counts itself plus its children, a <c>repeat</c> counts <c>max</c> times its body
/// plus one, a <c>when</c> counts its body plus one, and a <c>use</c> counts the whole used
/// body plus one.
/// </summary>
internal static class CompositeStructure
{
    /// <summary>Adds the ids a body uses, including those inside <c>fill</c> bodies.</summary>
    internal static void Dependencies(ComponentNode node, HashSet<string> found)
    {
        switch (node)
        {
            case LayoutNode layout:
                foreach (ComponentNode child in layout.Children)
                {
                    Dependencies(child, found);
                }
                break;
            case RepeatNode repeat:
                Dependencies(repeat.Body, found);
                break;
            case WhenNode whenNode:
                Dependencies(whenNode.Then, found);
                break;
            case UseNode use:
                found.Add(use.Component);
                foreach (ComponentNode body in use.Fill.Values)
                {
                    Dependencies(body, found);
                }
                break;
            default:
                break;
        }
    }

    /// <summary>The components that sit on a cycle of the reference graph.</summary>
    internal static HashSet<string> Cycles(IReadOnlyDictionary<string, ComponentSpec> components)
    {
        ArgumentNullException.ThrowIfNull(components);
        Dictionary<string, HashSet<string>> graph = new(StringComparer.Ordinal);
        foreach ((string id, ComponentSpec component) in components)
        {
            HashSet<string> edges = new(StringComparer.Ordinal);
            Dependencies(component.Body, edges);
            graph[id] = edges;
        }
        Dictionary<string, int> state = new(StringComparer.Ordinal);
        HashSet<string> cyclic = new(StringComparer.Ordinal);
        foreach (string id in graph.Keys)
        {
            if (!state.ContainsKey(id))
            {
                Visit(id, graph, state, cyclic);
            }
        }
        return cyclic;
    }

    private static void Visit(string id, Dictionary<string, HashSet<string>> graph, Dictionary<string, int> state, HashSet<string> cyclic)
    {
        state[id] = 1;
        if (graph.TryGetValue(id, out HashSet<string>? edges))
        {
            foreach (string edge in edges)
            {
                if (!state.TryGetValue(edge, out int seen))
                {
                    Visit(edge, graph, state, cyclic);
                }
                else if (seen == 1)
                {
                    cyclic.Add(id);
                    cyclic.Add(edge);
                }
            }
        }
        state[id] = 2;
    }

    /// <summary>
    /// The (depth, node count) of expanding <paramref name="body" />, or null when a
    /// <c>use</c> names an absent component or the graph loops (reported separately).
    /// </summary>
    internal static (int Depth, int Nodes)? Measure(ComponentNode body, IReadOnlyDictionary<string, ComponentSpec> components)
    {
        ArgumentNullException.ThrowIfNull(body);
        ArgumentNullException.ThrowIfNull(components);
        return MeasureCore(body, components, new Dictionary<string, (int Depth, int Nodes)>(StringComparer.Ordinal), []);
    }

    private static (int Depth, int Nodes)? MeasureCore(
        ComponentNode node,
        IReadOnlyDictionary<string, ComponentSpec> components,
        Dictionary<string, (int Depth, int Nodes)> memo,
        List<string> active)
    {
        switch (node)
        {
            case LayoutNode layout:
                {
                    int depth = 0;
                    int nodes = 1;
                    foreach (ComponentNode child in layout.Children)
                    {
                        (int Depth, int Nodes)? measured = MeasureCore(child, components, memo, active);
                        if (measured is null)
                        {
                            return null;
                        }
                        depth = Math.Max(depth, measured.Value.Depth);
                        nodes = Saturating(nodes, measured.Value.Nodes);
                    }
                    return (SaturatingOne(depth), nodes);
                }
            case RepeatNode repeat:
                {
                    (int Depth, int Nodes)? body = MeasureCore(repeat.Body, components, memo, active);
                    if (body is null)
                    {
                        return null;
                    }
                    return (SaturatingOne(body.Value.Depth), Saturating(body.Value.Nodes * repeat.Max, 1));
                }
            case WhenNode whenNode:
                {
                    (int Depth, int Nodes)? then = MeasureCore(whenNode.Then, components, memo, active);
                    if (then is null)
                    {
                        return null;
                    }
                    return (SaturatingOne(then.Value.Depth), Saturating(then.Value.Nodes, 1));
                }
            case UseNode use:
                {
                    if (active.Contains(use.Component, StringComparer.Ordinal) || !components.TryGetValue(use.Component, out ComponentSpec? used))
                    {
                        return null;
                    }
                    (int Depth, int Nodes) body;
                    if (memo.TryGetValue(use.Component, out (int Depth, int Nodes) cached))
                    {
                        body = cached;
                    }
                    else
                    {
                        active.Add(use.Component);
                        (int Depth, int Nodes)? measured = MeasureCore(used.Body, components, memo, active);
                        active.RemoveAt(active.Count - 1);
                        if (measured is null)
                        {
                            return null;
                        }
                        body = measured.Value;
                        memo[use.Component] = body;
                    }
                    int depth = body.Depth;
                    int nodes = body.Nodes;
                    foreach (ComponentNode filled in use.Fill.Values)
                    {
                        (int Depth, int Nodes)? slot = MeasureCore(filled, components, memo, active);
                        if (slot is null)
                        {
                            return null;
                        }
                        depth = Math.Max(depth, slot.Value.Depth);
                        nodes = Saturating(nodes, slot.Value.Nodes);
                    }
                    return (SaturatingOne(depth), Saturating(nodes, 1));
                }
            default:
                return (1, 1);
        }
    }

    private static int Saturating(int value, int add) =>
        value > int.MaxValue - add ? int.MaxValue : value + add;

    private static int SaturatingOne(int value) => Saturating(value, 1);
}
