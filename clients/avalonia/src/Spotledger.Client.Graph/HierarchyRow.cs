namespace Spotledger.Client.Graph;

/// <summary>One visible line of the flattened tree: the node, its indentation depth and the
/// expansion state that produced it. The control paints rows, it does not create them.</summary>
/// <param name="Node">The node this row shows.</param>
/// <param name="Depth">The zero-based indentation depth.</param>
/// <param name="IsExpanded">Whether the node's loaded children follow this row.</param>
/// <param name="IsLoadingChildren">Whether a child page is in flight for this node.</param>
/// <param name="SiblingIndex">The node's position among its siblings in the current order.</param>
public sealed record HierarchyRow(
    HierarchyNode Node,
    int Depth,
    bool IsExpanded,
    bool IsLoadingChildren = false,
    int SiblingIndex = 0)
{
    /// <summary>The node's key.</summary>
    public GraphNodeKey Key => Node.Key;

    /// <summary>The title to draw (the locked placeholder when redacted).</summary>
    public string Title => Node.DisplayTitle;

    /// <summary>The detail to draw (empty when redacted).</summary>
    public string Detail => Node.DisplayDetail;
}

/// <summary>One step of the breadcrumb trail above the tree.</summary>
/// <param name="Key">The node.</param>
/// <param name="Title">The title to draw (the locked placeholder when redacted).</param>
public sealed record HierarchyBreadcrumb(GraphNodeKey Key, string Title);
