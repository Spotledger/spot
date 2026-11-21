namespace Spotledger.Client.Graph;

/// <summary>Deterministic sample hierarchies: the gallery fixture and the 10,000-node
/// performance workload. Titles carry a zero-padded record number so type-ahead and sort
/// order are unambiguous.</summary>
public static class HierarchySamples
{
    /// <summary>Builds a deterministic account tree of exactly <paramref name="count"/> nodes.
    /// Node 0 is the only root; every other node's parent is <c>(i - 1) / branching</c>.</summary>
    /// <param name="count">Total nodes (including the root).</param>
    /// <param name="branching">Children per node (the fan-out that makes wide pages).</param>
    public static IReadOnlyList<HierarchyNode> Accounts(int count, int branching = 8)
    {
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(count);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(branching);
        var nodes = new List<HierarchyNode>(count);
        for (var i = 0; i < count; i++)
        {
            var key = new GraphNodeKey("Account", i.ToString("D6", System.Globalization.CultureInfo.InvariantCulture));
            GraphNodeKey? parent = i == 0 ? null : new GraphNodeKey("Account", ((i - 1) / branching).ToString("D6", System.Globalization.CultureInfo.InvariantCulture));
            var hasChildren = i * branching + 1 < count;
            nodes.Add(new HierarchyNode(
                key,
                parent,
                string.Concat("Account ", i.ToString("D6", System.Globalization.CultureInfo.InvariantCulture)),
                "Account",
                hasChildren,
                IsLocked: false,
                Detail: i % 7 == 0 ? "group" : null));
        }

        return nodes;
    }

    /// <summary>An in-memory source over <see cref="Accounts"/>.</summary>
    /// <param name="count">Total nodes.</param>
    /// <param name="branching">Children per node.</param>
    /// <param name="gate">Optional await before each page.</param>
    public static InMemoryHierarchySource AccountsSource(
        int count,
        int branching = 8,
        Func<HierarchyRequest, CancellationToken, Task>? gate = null)
        => new(Accounts(count, branching), gate);

    /// <summary>A small organisation tree with one redacted node, for the locked-placeholder
    /// flows: the caller may see that the child exists but not what it is.</summary>
    public static IReadOnlyList<HierarchyNode> Organisation()
    {
        var head = new GraphNodeKey("Company", "head");
        var ops = new GraphNodeKey("Department", "ops");
        var finance = new GraphNodeKey("Department", "finance");
        var hidden = new GraphNodeKey("Department", "hidden");
        var hiddenChild = new GraphNodeKey("Team", "hidden-team");
        return
        [
            new HierarchyNode(head, null, "Spotledger Holdings", "Company", true),
            new HierarchyNode(ops, head, "Operations", "Department", false, Detail: "12 people"),
            new HierarchyNode(finance, head, "Finance", "Department", true, Detail: "6 people"),
            new HierarchyNode(new GraphNodeKey("Team", "ledger"), finance, "Ledger", "Team", false),
            HierarchyNode.Locked(hidden, head, "Department", hasChildren: true),
            HierarchyNode.Locked(hiddenChild, hidden, "Team"),
        ];
    }
}
