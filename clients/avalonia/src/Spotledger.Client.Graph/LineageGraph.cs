namespace Spotledger.Client.Graph;

/// <summary>One card in a lineage view: the document's title, lifecycle status and amount,
/// exactly as the list model formatted them (the view never does money arithmetic).</summary>
/// <param name="Key">The record.</param>
/// <param name="Title">The document title.</param>
/// <param name="Status">The lifecycle status caption (Draft, Posted, Cancelled …).</param>
/// <param name="Amount">The amount display string, already formatted by the list model.</param>
/// <param name="IsLocked">Whether the caller may not read this record's content.</param>
public sealed record LineageNode(
    GraphNodeKey Key,
    string Title,
    string Status,
    string Amount,
    bool IsLocked = false)
{
    /// <summary>The text a locked card shows instead of its content.</summary>
    public const string LockedTitle = "Locked item";

    /// <summary>The title to draw (the locked placeholder when redacted).</summary>
    public string DisplayTitle => IsLocked ? LockedTitle : Title;
}

/// <summary>One directed relationship between two lineage cards.</summary>
/// <param name="From">The source node.</param>
/// <param name="To">The target node.</param>
/// <param name="Label">The edge-type caption (Order to Receipt, Receipt to Invoice …).</param>
public sealed record LineageEdge(GraphNodeKey From, GraphNodeKey To, string Label);

/// <summary>The visible slice of a graph query result, handed to the view by the typed
/// client: nodes in the query's order and the edges that connect them.</summary>
/// <param name="Nodes">The nodes, in the query's order (the layout's deterministic tie-break).</param>
/// <param name="Edges">The edges among those nodes.</param>
public sealed record GraphSnapshot(IReadOnlyList<LineageNode> Nodes, IReadOnlyList<LineageEdge> Edges)
{
    /// <summary>An empty snapshot.</summary>
    public static GraphSnapshot Empty { get; } = new([], []);
}

/// <summary>A point in layout space (device-independent pixels; not money or time).</summary>
/// <param name="X">The horizontal coordinate.</param>
/// <param name="Y">The vertical coordinate.</param>
public readonly record struct LineagePoint(double X, double Y);

/// <summary>Where a node's card sits: its layer (column), row and top-left position.</summary>
/// <param name="Node">The node.</param>
/// <param name="Layer">The zero-based column.</param>
/// <param name="Row">The zero-based row inside the column.</param>
/// <param name="Position">The card's top-left corner in layout space.</param>
public sealed record LineageNodePlacement(LineageNode Node, int Layer, int Row, LineagePoint Position)
{
    /// <summary>The card's key.</summary>
    public GraphNodeKey Key => Node.Key;
}

/// <summary>An edge's orthogonal route: source anchor, mid-points, target anchor.</summary>
/// <param name="From">The source node key.</param>
/// <param name="To">The target node key.</param>
/// <param name="Label">The edge label.</param>
/// <param name="Points">The polyline, source first.</param>
public sealed record LineageEdgeRoute(GraphNodeKey From, GraphNodeKey To, string Label, IReadOnlyList<LineagePoint> Points);

/// <summary>A computed layout: deterministic positions for every node and a route for every
/// edge, with the extent the view needs for panning.</summary>
/// <param name="Nodes">The placements, in the snapshot's node order.</param>
/// <param name="Edges">The edge routes.</param>
/// <param name="LayerCount">The number of layers (columns).</param>
/// <param name="Width">The layout's extent width.</param>
/// <param name="Height">The layout's extent height.</param>
/// <param name="ComputedOnThreadId">The managed thread the computation ran on (evidence that a
/// large graph was laid out off the UI thread).</param>
public sealed record LineageLayout(
    IReadOnlyList<LineageNodePlacement> Nodes,
    IReadOnlyList<LineageEdgeRoute> Edges,
    int LayerCount,
    double Width,
    double Height,
    int ComputedOnThreadId)
{
    /// <summary>An empty layout.</summary>
    public static LineageLayout Empty { get; } =
        new([], [], 0, LineageLayoutEngine.Padding * 2, LineageLayoutEngine.Padding * 2, 0);

    /// <summary>Finds a placement by key.</summary>
    /// <param name="key">The node.</param>
    public LineageNodePlacement? Find(GraphNodeKey key)
    {
        foreach (var placement in Nodes)
        {
            if (placement.Key == key)
            {
                return placement;
            }
        }

        return null;
    }
}
