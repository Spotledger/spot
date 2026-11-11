namespace Spotledger.Client.Graph;

/// <summary>
/// The flattened, virtualised tree behind <see cref="SpotHierarchy"/>. It holds only the
/// pages the user revealed: the root page, then one page per expanded level, then one more
/// page when the viewport reaches the end of a loaded page. Every operation is synchronous
/// over that loaded set except the page fetches, so keyboard navigation never waits on the
/// network. Sorting and filtering are pushed down to the source with each request; changing
/// either reloads the tree and collapses it.
/// </summary>
public sealed partial class HierarchyModel : IDisposable
{
    private readonly List<HierarchyNode> _roots = [];
    private readonly Dictionary<GraphNodeKey, List<HierarchyNode>> _children = [];
    private readonly Dictionary<GraphNodeKey, bool> _childrenHaveMore = [];
    private readonly HashSet<GraphNodeKey> _loadingChildren = [];
    private readonly HashSet<GraphNodeKey> _expanded = [];
    private readonly Dictionary<GraphNodeKey, HierarchyNode> _index = [];
    private readonly List<HierarchyRow> _rows = [];
    private bool _rowsDirty;
    private bool _rootsLoaded;
    private bool _rootsHaveMore;
    private bool _rootLoading;

    /// <summary>Creates the model over a source. <paramref name="pageSize"/> is the page
    /// size for every level (children load in pages of this many nodes).</summary>
    /// <param name="source">The paged hierarchy.</param>
    /// <param name="pageSize">Nodes per page.</param>
    public HierarchyModel(IHierarchySource source, int pageSize = 100)
    {
        ArgumentNullException.ThrowIfNull(source);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(pageSize);
        Source = source;
        PageSize = pageSize;
        Source.Changed += OnSourceChanged;
    }

    /// <summary>The paged hierarchy.</summary>
    public IHierarchySource Source { get; }

    /// <summary>Nodes fetched per request.</summary>
    public int PageSize { get; }

    /// <summary>The current sort and filter.</summary>
    public HierarchyQuery Query { get; private set; } = HierarchyQuery.Default;

    /// <summary>Selection keyed by node.</summary>
    public HierarchySelection Selection { get; } = new();

    /// <summary>The focused node, or null when nothing is focused.</summary>
    public GraphNodeKey? Cursor { get; private set; }

    /// <summary>The last load failure, or null.</summary>
    public string? Error { get; private set; }

    /// <summary>True once the first root page arrived.</summary>
    public bool IsInitialized { get; private set; }

    /// <summary>True while the first root page is in flight.</summary>
    public bool IsRootLoading => _rootLoading;

    /// <summary>True when the root level holds another page.</summary>
    public bool RootsHaveMore => _rootsHaveMore;

    /// <summary>The number of top-level nodes loaded so far.</summary>
    public int LoadedRootCount => _roots.Count;

    /// <summary>Raised when the visible rows, a loading state or the error state changed.</summary>
    public event EventHandler? Changed;

    /// <summary>Every visible row, flattened depth-first. Rows are rebuilt lazily after a
    /// change, so repeated reads (one paint per frame) do no work.</summary>
    public IReadOnlyList<HierarchyRow> Rows
    {
        get
        {
            RebuildRows();
            return _rows;
        }
    }

    /// <summary>The number of visible rows.</summary>
    public int RowCount => Rows.Count;

    /// <summary>Loads the first root page. Safe to call twice (the second is a no-op).</summary>
    /// <param name="cancellationToken">Cancels the fetch.</param>
    public async Task InitializeAsync(CancellationToken cancellationToken)
    {
        if (IsInitialized || _rootLoading)
        {
            return;
        }

        await LoadRootsAsync(0, cancellationToken).ConfigureAwait(true);
    }

    /// <summary>Reloads everything the view holds, keeping the cursor when the node survives.</summary>
    /// <param name="cancellationToken">Cancels the fetch.</param>
    public async Task RefreshAsync(CancellationToken cancellationToken)
    {
        var cursor = Cursor;
        _roots.Clear();
        _children.Clear();
        _childrenHaveMore.Clear();
        _loadingChildren.Clear();
        _expanded.Clear();
        _index.Clear();
        _rowsDirty = true;
        _rootsLoaded = false;
        _rootsHaveMore = false;
        IsInitialized = false;
        Cursor = cursor;
        RaiseChanged();
        await InitializeAsync(cancellationToken).ConfigureAwait(true);
    }

    /// <summary>Applies a new sort or filter: the tree reloads and collapses, because the
    /// pages it held were answered under the old query.</summary>
    /// <param name="query">The new sort and filter.</param>
    /// <param name="cancellationToken">Cancels the fetch.</param>
    public async Task SetQueryAsync(HierarchyQuery query, CancellationToken cancellationToken)
    {
        if (query == Query)
        {
            return;
        }

        Query = query;
        Selection.Clear();
        Cursor = null;
        await RefreshAsync(cancellationToken).ConfigureAwait(true);
    }

    /// <summary>Reloads the pages the view holds while keeping every expansion and the cursor.
    /// Used after an edit the server confirmed (a re-parent) so the tree does not collapse
    /// under the user's hands; <see cref="RefreshAsync"/> stays the full-reset path.</summary>
    /// <param name="cancellationToken">Cancels the fetches.</param>
    public async Task ReloadAsync(CancellationToken cancellationToken)
    {
        var expanded = new List<GraphNodeKey>(_expanded);
        var loadedCounts = new Dictionary<GraphNodeKey, int>(_children.Count);
        foreach (var pair in _children)
        {
            loadedCounts[pair.Key] = pair.Value.Count;
        }

        var cursor = Cursor;
        _roots.Clear();
        _children.Clear();
        _childrenHaveMore.Clear();
        _loadingChildren.Clear();
        _expanded.Clear();
        _index.Clear();
        _rowsDirty = true;
        _rootsLoaded = false;
        _rootsHaveMore = false;
        IsInitialized = false;
        RaiseChanged();

        await LoadRootsAsync(0, cancellationToken).ConfigureAwait(true);

        // Breadth-first: a parent's page must load before its expanded children are known.
        var pending = expanded;
        var progressed = true;
        while (pending.Count > 0 && progressed)
        {
            progressed = false;
            for (var i = pending.Count - 1; i >= 0; i--)
            {
                var key = pending[i];
                if (FindNode(key) is not { HasChildren: true })
                {
                    pending.RemoveAt(i);
                    continue;
                }

                _expanded.Add(key);
                await LoadChildrenAsync(key, 0, cancellationToken).ConfigureAwait(true);
                if (loadedCounts.TryGetValue(key, out var count) && count > PageSize)
                {
                    await LoadChildrenAsync(key, PageSize, cancellationToken).ConfigureAwait(true);
                }

                pending.RemoveAt(i);
                progressed = true;
            }
        }

        Cursor = cursor;
        _rowsDirty = true;
        RaiseChanged();
    }

    /// <summary>Expands a node: marks it expanded and fetches its first child page when it
    /// has children and no page is loaded yet.</summary>
    /// <param name="key">The node.</param>
    /// <param name="cancellationToken">Cancels the fetch.</param>
    public async Task ExpandAsync(GraphNodeKey key, CancellationToken cancellationToken)
    {
        if (FindNode(key) is not { HasChildren: true } node)
        {
            return;
        }

        _expanded.Add(key);
        _rowsDirty = true;
        RaiseChanged();
        if (!_children.ContainsKey(key))
        {
            await LoadChildrenAsync(key, 0, cancellationToken).ConfigureAwait(true);
        }
    }

    /// <summary>Collapses a node; a cursor inside the collapsed subtree moves up to the node.</summary>
    /// <param name="key">The node.</param>
    public void Collapse(GraphNodeKey key)
    {
        if (!_expanded.Remove(key))
        {
            return;
        }

        if (Cursor is { } cursor && IsDescendantOf(cursor, key))
        {
            Cursor = key;
        }

        _rowsDirty = true;
        RaiseChanged();
    }

    /// <summary>Expands or collapses a node.</summary>
    /// <param name="key">The node.</param>
    /// <param name="cancellationToken">Cancels the fetch.</param>
    public Task ToggleAsync(GraphNodeKey key, CancellationToken cancellationToken)
    {
        if (_expanded.Contains(key))
        {
            Collapse(key);
            return Task.CompletedTask;
        }

        return ExpandAsync(key, cancellationToken);
    }

    /// <summary>Expands every sibling of a node (the tree's <c>*</c> key), one page each.</summary>
    /// <param name="key">A node whose siblings to expand.</param>
    /// <param name="cancellationToken">Cancels the fetches.</param>
    public async Task ExpandAllSiblingsAsync(GraphNodeKey key, CancellationToken cancellationToken)
    {
        if (FindNode(key) is not { } node)
        {
            return;
        }

        var siblings = SiblingsOf(node);
        for (var i = 0; i < siblings.Count; i++)
        {
            if (siblings[i].HasChildren)
            {
                await ExpandAsync(siblings[i].Key, cancellationToken).ConfigureAwait(true);
            }
        }
    }

    /// <summary>Loads the next page for whatever level the viewport just reached the end of:
    /// the root level, or the parent of the last visible row. This is what keeps a level of
    /// 10,000 siblings paged while the user scrolls.</summary>
    /// <param name="firstRow">The first visible row index.</param>
    /// <param name="rowCount">How many rows fit the viewport.</param>
    /// <param name="cancellationToken">Cancels the fetch.</param>
    public async Task LoadMoreForWindowAsync(int firstRow, int rowCount, CancellationToken cancellationToken)
    {
        var rows = Rows;
        if (rows.Count == 0)
        {
            return;
        }

        var last = Math.Min(rows.Count - 1, Math.Max(0, firstRow) + Math.Max(1, rowCount) - 1);
        var row = rows[last];
        if (row.Depth == 0)
        {
            // Only the page boundary the viewport actually reached is fetched: the last
            // visible root must be the last loaded root, or a short viewport would pull every
            // page of a 10,000-row level at once.
            if (_rootsHaveMore && _rootsLoaded && !_rootLoading && row.SiblingIndex >= _roots.Count - 1)
            {
                await LoadRootsAsync(_roots.Count, cancellationToken).ConfigureAwait(true);
            }

            return;
        }

        if (row.Node.Parent is not { } parent)
        {
            return;
        }

        if (_childrenHaveMore.TryGetValue(parent, out var more) && more
            && _children.TryGetValue(parent, out var loaded) && !_loadingChildren.Contains(parent)
            && row.SiblingIndex >= loaded.Count - 1)
        {
            await LoadChildrenAsync(parent, loaded.Count, cancellationToken).ConfigureAwait(true);
        }
    }

    /// <summary>Finds a loaded node by key.</summary>
    /// <param name="key">The node.</param>
    public HierarchyNode? FindNode(GraphNodeKey key) => _index.TryGetValue(key, out var node) ? node : null;

    /// <summary>True when the node's children are expanded.</summary>
    /// <param name="key">The node.</param>
    public bool IsExpanded(GraphNodeKey key) => _expanded.Contains(key);

    /// <summary>True when at least one child page is loaded for the node.</summary>
    /// <param name="key">The node.</param>
    public bool HasLoadedChildren(GraphNodeKey key) => _children.ContainsKey(key);

    /// <summary>The loaded children of a node, in order (empty when none).</summary>
    /// <param name="key">The node.</param>
    public IReadOnlyList<HierarchyNode> LoadedChildren(GraphNodeKey key)
        => _children.TryGetValue(key, out var children) ? children : [];

    /// <summary>The index of a row by key, or -1 when the node is not visible.</summary>
    /// <param name="key">The node.</param>
    public int IndexOf(GraphNodeKey key)
    {
        var rows = Rows;
        for (var i = 0; i < rows.Count; i++)
        {
            if (rows[i].Key == key)
            {
                return i;
            }
        }

        return -1;
    }

    /// <summary>The breadcrumb trail of a node: root first, the node last. Walks the parent
    /// links of the nodes the view loaded; a parent outside the loaded set ends the trail.</summary>
    /// <param name="key">The node, or null for an empty trail.</param>
    public IReadOnlyList<HierarchyBreadcrumb> Breadcrumbs(GraphNodeKey? key)
    {
        if (key is not { } start || !_index.ContainsKey(start))
        {
            return [];
        }

        var trail = new List<HierarchyBreadcrumb>();
        var current = start;
        var guard = new HashSet<GraphNodeKey>();
        while (guard.Add(current) && _index.TryGetValue(current, out var node))
        {
            trail.Add(new HierarchyBreadcrumb(node.Key, node.DisplayTitle));
            if (node.Parent is not { } parent)
            {
                break;
            }

            current = parent;
        }

        trail.Reverse();
        return trail;
    }

    /// <summary>Moves the cursor to a node without scrolling; the control scrolls to reveal it.</summary>
    /// <param name="key">The node, or null to clear the cursor.</param>
    public void MoveCursorTo(GraphNodeKey? key)
    {
        if (Cursor == key)
        {
            return;
        }

        Cursor = key;
        RaiseChanged();
    }

    /// <summary>True when <paramref name="candidate"/> sits below <paramref name="ancestor"/>
    /// in the loaded parent links. Presentation only (breadcrumbs and collapse); validity of a
    /// re-parent is never decided here.</summary>
    /// <param name="candidate">The possible descendant.</param>
    /// <param name="ancestor">The possible ancestor.</param>
    public bool IsDescendantOf(GraphNodeKey candidate, GraphNodeKey ancestor)
    {
        var current = candidate;
        var guard = new HashSet<GraphNodeKey>();
        while (guard.Add(current))
        {
            if (current == ancestor)
            {
                return true;
            }

            if (!_index.TryGetValue(current, out var node) || node.Parent is not { } parent)
            {
                return false;
            }

            current = parent;
        }

        return false;
    }

    private List<HierarchyNode> SiblingsOf(HierarchyNode node)
    {
        if (node.Parent is not { } parent)
        {
            return _roots;
        }

        return _children.TryGetValue(parent, out var children) ? children : [];
    }

    private async Task LoadRootsAsync(int start, CancellationToken cancellationToken)
    {
        _rootLoading = true;
        Error = null;
        RaiseChanged();
        try
        {
            var page = await Source.GetRoots(new HierarchyRequest(Query, start, PageSize), cancellationToken)
                .ConfigureAwait(true);
            if (start == 0)
            {
                _roots.Clear();
            }

            for (var i = 0; i < page.Nodes.Count; i++)
            {
                var node = page.Nodes[i];
                _roots.Add(node);
                _index[node.Key] = node;
            }

            _rootsHaveMore = page.HasMore;
            _rootsLoaded = true;
            IsInitialized = true;
            if (Cursor is null && _roots.Count > 0)
            {
                Cursor = _roots[0].Key;
            }
        }
        catch (OperationCanceledException)
        {
            return;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            Error = error.Message;
        }
        finally
        {
            _rootLoading = false;
            _rowsDirty = true;
            RaiseChanged();
        }
    }

    private async Task LoadChildrenAsync(GraphNodeKey parent, int start, CancellationToken cancellationToken)
    {
        if (!_loadingChildren.Add(parent))
        {
            return;
        }

        Error = null;
        _rowsDirty = true;
        RaiseChanged();
        try
        {
            var page = await Source.GetChildren(parent, new HierarchyRequest(Query, start, PageSize), cancellationToken)
                .ConfigureAwait(true);
            if (!_children.TryGetValue(parent, out var children))
            {
                children = [];
                _children.Add(parent, children);
            }

            if (start == 0)
            {
                children.Clear();
            }

            for (var i = 0; i < page.Nodes.Count; i++)
            {
                var node = page.Nodes[i];
                children.Add(node);
                _index[node.Key] = node;
            }

            _childrenHaveMore[parent] = page.HasMore;
        }
        catch (OperationCanceledException)
        {
            return;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            Error = error.Message;
        }
        finally
        {
            _loadingChildren.Remove(parent);
            _rowsDirty = true;
            RaiseChanged();
        }
    }

    private void OnSourceChanged(object? sender, EventArgs e)
    {
        _ = RefreshFromSourceAsync();
    }

    private async Task RefreshFromSourceAsync()
    {
        try
        {
            await ReloadAsync(CancellationToken.None).ConfigureAwait(true);
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            Error = error.Message;
            RaiseChanged();
        }
    }

    private void RebuildRows()
    {
        if (!_rowsDirty)
        {
            return;
        }

        _rows.Clear();
        var visited = new HashSet<GraphNodeKey>();
        for (var i = 0; i < _roots.Count; i++)
        {
            Append(_roots[i], 0, i, visited);
        }

        Selection.Retain(_rows);
        _rowsDirty = false;
    }

    private void Append(HierarchyNode node, int depth, int siblingIndex, HashSet<GraphNodeKey> visited)
    {
        if (!visited.Add(node.Key))
        {
            return;
        }

        var expanded = _expanded.Contains(node.Key);
        _rows.Add(new HierarchyRow(node, depth, expanded, _loadingChildren.Contains(node.Key), siblingIndex));
        if (!expanded || !_children.TryGetValue(node.Key, out var children))
        {
            return;
        }

        for (var i = 0; i < children.Count; i++)
        {
            Append(children[i], depth + 1, i, visited);
        }
    }

    /// <summary>Unsubscribes from the source. The control owns one model per source and
    /// disposes it when the source changes.</summary>
    public void Dispose() => Source.Changed -= OnSourceChanged;

    private void RaiseChanged() => Changed?.Invoke(this, EventArgs.Empty);
}
