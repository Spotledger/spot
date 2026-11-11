namespace Spotledger.Client.Graph;

/// <summary>
/// The paged hierarchy the tree renders against. The view never holds the whole tree: it
/// asks for the roots, then for one page of children when a node is expanded, then for the
/// next page when the viewport reaches the end of a loaded page. Sort and filter travel with
/// every request (the graph query answers them), and a locked node arrives already redacted.
/// Implementations complete synchronously or asynchronously; the view never blocks on them.
/// </summary>
public interface IHierarchySource
{
    /// <summary>Fetches one page of top-level nodes.</summary>
    /// <param name="request">The page offset, size, sort and filter.</param>
    /// <param name="cancellationToken">Cancels the fetch when a newer one supersedes it.</param>
    /// <returns>The page, with <see cref="HierarchyPage.HasMore"/> set when another follows.</returns>
    Task<HierarchyPage> GetRoots(HierarchyRequest request, CancellationToken cancellationToken);

    /// <summary>Fetches one page of a node's direct children, in the query's order.</summary>
    /// <param name="parent">The node whose children to fetch.</param>
    /// <param name="request">The page offset, size, sort and filter.</param>
    /// <param name="cancellationToken">Cancels the fetch when a newer one supersedes it.</param>
    /// <returns>The page, with <see cref="HierarchyPage.HasMore"/> set when another follows.</returns>
    Task<HierarchyPage> GetChildren(GraphNodeKey parent, HierarchyRequest request, CancellationToken cancellationToken);

    /// <summary>Raised when the underlying data changed; the view reloads what it shows.</summary>
    event EventHandler? Changed;
}
