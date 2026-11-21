namespace Spotledger.Client.Graph;

/// <summary>One node of a hierarchy, as the graph query returned it to this caller.
/// A node the caller may not read never arrives here: the query API removes it and sets
/// its truncation flag. A node the caller may read but whose title is redacted arrives
/// with <see cref="IsLocked"/> set and an empty <see cref="Title"/>, so neither the drawn
/// row nor an accessible name can leak text the caller may not see.</summary>
/// <param name="Key">The record.</param>
/// <param name="Parent">The parent record when the view knows it (null for a root).</param>
/// <param name="Title">The document title, or empty for a locked placeholder.</param>
/// <param name="Kind">The entity name, shown as the row's kind caption.</param>
/// <param name="HasChildren">Whether the server says children exist (draws the expander).</param>
/// <param name="IsLocked">Whether the title (and detail) is redacted for this caller.</param>
/// <param name="Detail">An optional display string drawn at the row's trailing edge.</param>
public sealed record HierarchyNode(
    GraphNodeKey Key,
    GraphNodeKey? Parent,
    string Title,
    string Kind,
    bool HasChildren,
    bool IsLocked = false,
    string? Detail = null)
{
    /// <summary>The text a locked placeholder shows instead of a title.</summary>
    public const string LockedTitle = "Locked item";

    /// <summary>The text a locked placeholder shows instead of a detail.</summary>
    public const string LockedDetail = "";

    /// <summary>The title to draw: the real title, or the locked placeholder text.</summary>
    public string DisplayTitle => IsLocked ? LockedTitle : Title;

    /// <summary>The detail to draw; empty for a locked placeholder.</summary>
    public string DisplayDetail => IsLocked ? LockedDetail : Detail ?? string.Empty;

    /// <summary>A redacted placeholder: the caller knows a child exists but may not read it.</summary>
    public static HierarchyNode Locked(GraphNodeKey key, GraphNodeKey? parent, string kind, bool hasChildren = false)
        => new(key, parent, string.Empty, kind, hasChildren, IsLocked: true);
}

/// <summary>One page of a hierarchy level, in the query's order.</summary>
/// <param name="Nodes">The nodes in this page.</param>
/// <param name="HasMore">Whether the level holds another page after this one.</param>
public sealed record HierarchyPage(IReadOnlyList<HierarchyNode> Nodes, bool HasMore)
{
    /// <summary>An empty page.</summary>
    public static HierarchyPage Empty { get; } = new([], false);
}

/// <summary>A paged request for one level of the hierarchy. Sorting and filtering travel
/// with the request: the source pushes them down to the graph query instead of the view
/// sorting a page it holds.</summary>
/// <param name="Query">The sort and filter the caller chose.</param>
/// <param name="Start">The first item offset within the level.</param>
/// <param name="Count">How many items to return.</param>
public readonly record struct HierarchyRequest(HierarchyQuery Query, int Start, int Count);

/// <summary>Which node property a hierarchy level is ordered by.</summary>
public enum HierarchySortField
{
    /// <summary>By title, the default.</summary>
    Title,
    /// <summary>By the entity name.</summary>
    Kind,
    /// <summary>By record id.</summary>
    Key,
}

/// <summary>The sort and filter pushed down to the graph query for every level.</summary>
/// <param name="Sort">The ordering property.</param>
/// <param name="Descending">True for a descending order.</param>
/// <param name="Filter">Case-insensitive title filter; null or blank keeps everything.</param>
public readonly record struct HierarchyQuery(HierarchySortField Sort, bool Descending, string? Filter)
{
    /// <summary>Title ascending, no filter.</summary>
    public static HierarchyQuery Default { get; } = new(HierarchySortField.Title, false, null);

    /// <summary>True when a non-blank filter is set.</summary>
    public bool HasFilter => !string.IsNullOrWhiteSpace(Filter);

    /// <summary>The same query with a different filter text.</summary>
    public HierarchyQuery WithFilter(string? filter) => this with { Filter = filter };
}
