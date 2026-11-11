namespace Spotledger.Client.Graph;

/// <summary>Selection over the visible tree, keyed by node. Keys survive re-sorting and
/// re-loading of pages, so a selection does not silently jump to a different node when the
/// order changes. The anchor is the row a shift-extension grew from.</summary>
public sealed class HierarchySelection
{
    private readonly HashSet<GraphNodeKey> _selected = [];

    /// <summary>How many nodes are selected.</summary>
    public int Count => _selected.Count;

    /// <summary>The key a shift-extension grows from (null before the first click).</summary>
    public GraphNodeKey? Anchor { get; private set; }

    /// <summary>The selected keys, in no particular order.</summary>
    public IReadOnlyCollection<GraphNodeKey> Keys => _selected;

    /// <summary>True when the node is selected.</summary>
    public bool Contains(GraphNodeKey key) => _selected.Contains(key);

    /// <summary>Replaces the selection with one node and moves the anchor to it.</summary>
    public void Select(GraphNodeKey key)
    {
        _selected.Clear();
        _selected.Add(key);
        Anchor = key;
    }

    /// <summary>Adds or removes one node without touching the rest; moves the anchor.</summary>
    public void Toggle(GraphNodeKey key)
    {
        if (!_selected.Remove(key))
        {
            _selected.Add(key);
        }

        Anchor = key;
    }

    /// <summary>Selects the visible span from the anchor to <paramref name="key"/>, both
    /// inclusive, replacing the selection. Without an anchor this is a plain select.</summary>
    public void ExtendTo(IReadOnlyList<HierarchyRow> rows, GraphNodeKey key)
    {
        if (Anchor is not { } anchor || anchor == key)
        {
            Select(key);
            return;
        }

        var from = IndexOf(rows, anchor);
        var to = IndexOf(rows, key);
        if (from < 0 || to < 0)
        {
            Select(key);
            return;
        }

        if (from > to)
        {
            (from, to) = (to, from);
        }

        _selected.Clear();
        for (var i = from; i <= to; i++)
        {
            _selected.Add(rows[i].Key);
        }
    }

    /// <summary>Selects every row given (used by select-all over the loaded view).</summary>
    public void SelectAll(IReadOnlyList<HierarchyRow> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        _selected.Clear();
        foreach (var row in rows)
        {
            _selected.Add(row.Key);
        }

        Anchor = rows.Count > 0 ? rows[0].Key : null;
    }

    /// <summary>Empties the selection and the anchor.</summary>
    public void Clear()
    {
        _selected.Clear();
        Anchor = null;
    }

    /// <summary>Drops keys the view no longer shows (after a reload or a collapse).</summary>
    public void Retain(IReadOnlyCollection<HierarchyRow> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        var visible = new HashSet<GraphNodeKey>(rows.Count);
        foreach (var row in rows)
        {
            visible.Add(row.Key);
        }

        _selected.RemoveWhere(key => !visible.Contains(key));
        if (Anchor is { } anchor && !visible.Contains(anchor))
        {
            Anchor = null;
        }
    }

    private static int IndexOf(IReadOnlyList<HierarchyRow> rows, GraphNodeKey key)
    {
        for (var i = 0; i < rows.Count; i++)
        {
            if (rows[i].Key == key)
            {
                return i;
            }
        }

        return -1;
    }
}
