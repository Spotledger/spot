namespace Spotledger.Client.Graph;

/// <summary>
/// An <see cref="IHierarchySource"/> over a flat parent/child node list — the gallery
/// backend and the test fixture. Sort and filter are answered here exactly as the graph
/// query would answer them (a filter keeps a node when its own title matches or any
/// descendant matches, so a filtered tree stays navigable), pages are sliced from the
/// ordered level, and an optional gate lets a test hold a page open and assert the loading
/// state. Locked nodes never match a title filter: matching their hidden title would leak it.
/// </summary>
public sealed class InMemoryHierarchySource : IHierarchySource
{
    private readonly List<HierarchyNode> _nodes;
    private readonly Dictionary<GraphNodeKey, List<HierarchyNode>> _byParent = [];
    private readonly Func<HierarchyRequest, CancellationToken, Task>? _gate;

    /// <summary>Creates a source over <paramref name="nodes"/>, which carry their parent links.</summary>
    /// <param name="nodes">The nodes, in caller order (the default ordering tie-break).</param>
    /// <param name="gate">An optional await before each page, for loading-state tests.</param>
    public InMemoryHierarchySource(IEnumerable<HierarchyNode> nodes, Func<HierarchyRequest, CancellationToken, Task>? gate = null)
    {
        ArgumentNullException.ThrowIfNull(nodes);
        _nodes = [.. nodes];
        _gate = gate;
        for (var i = 0; i < _nodes.Count; i++)
        {
            var node = _nodes[i];
            if (node.Parent is not { } parent)
            {
                continue;
            }

            if (!_byParent.TryGetValue(parent, out var children))
            {
                children = [];
                _byParent.Add(parent, children);
            }

            children.Add(node);
        }
    }

    /// <summary>Raised after <see cref="NotifyChanged"/>, exactly as a live source would.</summary>
    public event EventHandler? Changed;

    /// <summary>The node count the source holds.</summary>
    public int Count => _nodes.Count;

    /// <summary>Tells the view the data changed underneath it.</summary>
    public void NotifyChanged() => Changed?.Invoke(this, EventArgs.Empty);

    /// <inheritdoc />
    public async Task<HierarchyPage> GetRoots(HierarchyRequest request, CancellationToken cancellationToken)
    {
        if (_gate is not null)
        {
            await _gate(request, cancellationToken).ConfigureAwait(true);
        }

        var matching = Matching(request.Query);
        var level = new List<HierarchyNode>();
        foreach (var node in _nodes)
        {
            if (node.Parent is null && matching.Contains(node.Key))
            {
                level.Add(node);
            }
        }

        return Page(Sort(level, request.Query), request);
    }

    /// <inheritdoc />
    public async Task<HierarchyPage> GetChildren(GraphNodeKey parent, HierarchyRequest request, CancellationToken cancellationToken)
    {
        if (_gate is not null)
        {
            await _gate(request, cancellationToken).ConfigureAwait(true);
        }

        var matching = Matching(request.Query);
        var level = new List<HierarchyNode>();
        if (_byParent.TryGetValue(parent, out var children))
        {
            foreach (var node in children)
            {
                if (matching.Contains(node.Key))
                {
                    level.Add(node);
                }
            }
        }

        return Page(Sort(level, request.Query), request);
    }

    private static HierarchyPage Page(List<HierarchyNode> ordered, HierarchyRequest request)
    {
        var start = Math.Clamp(request.Start, 0, ordered.Count);
        var count = Math.Min(Math.Max(request.Count, 0), ordered.Count - start);
        var slice = new List<HierarchyNode>(count);
        for (var i = 0; i < count; i++)
        {
            slice.Add(ordered[start + i]);
        }

        return new HierarchyPage(slice, start + count < ordered.Count);
    }

    private HashSet<GraphNodeKey> Matching(HierarchyQuery query)
    {
        if (!query.HasFilter)
        {
            return [.. _nodes.Select(node => node.Key)];
        }

        var byKey = new Dictionary<GraphNodeKey, HierarchyNode>();
        foreach (var node in _nodes)
        {
            byKey[node.Key] = node;
        }

        var visible = new HashSet<GraphNodeKey>();
        foreach (var node in _nodes)
        {
            if (node.IsLocked || !node.Title.Contains(query.Filter!, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            var current = node;
            var guard = new HashSet<GraphNodeKey>();
            while (guard.Add(current.Key))
            {
                visible.Add(current.Key);
                if (current.Parent is not { } parent || !byKey.TryGetValue(parent, out var next))
                {
                    break;
                }

                current = next;
            }
        }

        return visible;
    }

    private static List<HierarchyNode> Sort(List<HierarchyNode> level, HierarchyQuery query)
    {
        var comparer = new NodeComparer(query.Sort, query.Descending);
        level.Sort(comparer);
        return level;
    }

    private sealed class NodeComparer(HierarchySortField field, bool descending) : IComparer<HierarchyNode>
    {
        public int Compare(HierarchyNode? x, HierarchyNode? y)
        {
            if (ReferenceEquals(x, y))
            {
                return 0;
            }

            if (x is null)
            {
                return -1;
            }

            if (y is null)
            {
                return 1;
            }

            var primary = field switch
            {
                HierarchySortField.Kind => string.Compare(x.Kind, y.Kind, StringComparison.OrdinalIgnoreCase),
                HierarchySortField.Key => string.Compare(x.Key.Id, y.Key.Id, StringComparison.Ordinal),
                _ => string.Compare(x.DisplayTitle, y.DisplayTitle, StringComparison.OrdinalIgnoreCase),
            };
            if (primary == 0)
            {
                primary = string.Compare(x.Key.ToString(), y.Key.ToString(), StringComparison.Ordinal);
            }

            return descending ? -primary : primary;
        }
    }
}
