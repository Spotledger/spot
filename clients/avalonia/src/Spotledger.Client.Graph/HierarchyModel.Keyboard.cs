namespace Spotledger.Client.Graph;

/// <summary>
/// The keyboard model of the tree: every operation is synchronous over the rows already
/// loaded, so arrows, Home/End, expand/collapse, jump and type-ahead never wait on the
/// network (the child page they reveal loads in the background). Type-ahead takes the
/// current instant as an argument: nothing in this model reads the wall clock.
/// </summary>
public sealed partial class HierarchyModel
{
    private const int TypeAheadWindowMs = 1000;
    private readonly System.Text.StringBuilder _typeAhead = new();
    private DateTimeOffset _typeAheadStamp = DateTimeOffset.MinValue;

    /// <summary>The cursor's row index, or -1 when it is unset or no longer visible.</summary>
    public int CursorIndex => Cursor is { } key ? IndexOf(key) : -1;

    /// <summary>Moves the cursor by <paramref name="delta"/> rows, clamped to the visible set.
    /// From no cursor it lands on the first row (forward) or the last (backward).</summary>
    /// <param name="delta">Rows to move; negative moves up.</param>
    public void MoveCursor(int delta)
    {
        var rows = Rows;
        if (rows.Count == 0)
        {
            Cursor = null;
            return;
        }

        var index = CursorIndex;
        index = index < 0
            ? delta >= 0 ? 0 : rows.Count - 1
            : Math.Clamp(index + delta, 0, rows.Count - 1);
        Cursor = rows[index].Key;
        RaiseChanged();
    }

    /// <summary>Moves the cursor to the first visible row.</summary>
    public void MoveToFirst() => MoveCursorToEdge(last: false);

    /// <summary>Moves the cursor to the last visible row.</summary>
    public void MoveToLast() => MoveCursorToEdge(last: true);

    /// <summary>Moves the cursor by one viewport of rows (PageUp/PageDown).</summary>
    /// <param name="rowsPerPage">The viewport's row count.</param>
    /// <param name="forward">True for PageDown.</param>
    public void PageMove(int rowsPerPage, bool forward)
    {
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(rowsPerPage);
        MoveCursor(forward ? rowsPerPage : -rowsPerPage);
    }

    /// <summary>Moves the cursor to the parent row when the cursor is on a child.</summary>
    /// <returns>True when the cursor moved.</returns>
    public bool MoveToParent()
    {
        var rows = Rows;
        var index = CursorIndex;
        if (index < 0 || rows[index].Depth == 0)
        {
            return false;
        }

        var depth = rows[index].Depth - 1;
        for (var i = index - 1; i >= 0; i--)
        {
            if (rows[i].Depth == depth)
            {
                Cursor = rows[i].Key;
                RaiseChanged();
                return true;
            }
        }

        return false;
    }

    /// <summary>Moves the cursor to the first child's row when the node is expanded.</summary>
    /// <returns>True when the cursor moved.</returns>
    public bool MoveToFirstChild()
    {
        var rows = Rows;
        var index = CursorIndex;
        if (index < 0 || index + 1 >= rows.Count || rows[index + 1].Depth != rows[index].Depth + 1)
        {
            return false;
        }

        Cursor = rows[index + 1].Key;
        RaiseChanged();
        return true;
    }

    /// <summary>The Left key: collapse the node when expanded, otherwise move to the parent.</summary>
    /// <param name="cancellationToken">Unused (no fetch happens); kept for the input table.</param>
    /// <returns>True when the key did something.</returns>
    public Task<bool> CollapseOrMoveToParentAsync(CancellationToken cancellationToken)
    {
        _ = cancellationToken;
        if (Cursor is not { } key || FindNode(key) is null)
        {
            return Task.FromResult(false);
        }

        if (IsExpanded(key))
        {
            Collapse(key);
            return Task.FromResult(true);
        }

        return Task.FromResult(MoveToParent());
    }

    /// <summary>The Right key: expand the node when collapsed, otherwise move to the first
    /// child.</summary>
    /// <param name="cancellationToken">Cancels the child page fetch.</param>
    /// <returns>True when the key did something.</returns>
    public async Task<bool> ExpandOrMoveToFirstChildAsync(CancellationToken cancellationToken)
    {
        if (Cursor is not { } key || FindNode(key) is not { HasChildren: true } node)
        {
            return false;
        }

        if (!IsExpanded(key))
        {
            await ExpandAsync(key, cancellationToken).ConfigureAwait(true);
            return true;
        }

        return MoveToFirstChild();
    }

    /// <summary>Selects the cursor row, replacing the selection; with
    /// <paramref name="extend"/> it grows the selection from the anchor.</summary>
    /// <param name="extend">True for a shift-extension.</param>
    public void SelectCursor(bool extend)
    {
        if (Cursor is not { } key)
        {
            return;
        }

        if (extend)
        {
            Selection.ExtendTo(Rows, key);
        }
        else
        {
            Selection.Select(key);
        }

        RaiseChanged();
    }

    /// <summary>Toggles the cursor row in the selection (the Space key).</summary>
    public void ToggleCursorSelection()
    {
        if (Cursor is not { } key)
        {
            return;
        }

        Selection.Toggle(key);
        RaiseChanged();
    }

    /// <summary>Selects every visible row (the select-all key).</summary>
    public void SelectAllVisible()
    {
        Selection.SelectAll(Rows);
        RaiseChanged();
    }

    /// <summary>
    /// Type-ahead: buffers the typed text and jumps the cursor to the next row whose title
    /// starts with the buffer, wrapping around. The buffer resets when more than a second
    /// passed since the previous character; <paramref name="now"/> is supplied by the caller.
    /// </summary>
    /// <param name="text">The typed text (one or more characters).</param>
    /// <param name="now">The current instant.</param>
    public void TypeAhead(string text, DateTimeOffset now)
    {
        if (string.IsNullOrEmpty(text) || text.Any(char.IsControl))
        {
            return;
        }

        if ((now - _typeAheadStamp).TotalMilliseconds > TypeAheadWindowMs)
        {
            _typeAhead.Clear();
        }

        _typeAhead.Append(text);
        _typeAheadStamp = now;
        JumpTypeAhead(_typeAhead.ToString());
    }

    /// <summary>The type-ahead buffer (tests read it between characters).</summary>
    public string TypeAheadBuffer => _typeAhead.ToString();

    /// <summary>Empties the type-ahead buffer (a focus change does this).</summary>
    public void ClearTypeAhead() => _typeAhead.Clear();

    /// <summary>The first move after <c>*</c>: expands the cursor's siblings (a Task the
    /// caller awaits). Same as <see cref="ExpandAllSiblingsAsync"/> but with no cursor.</summary>
    /// <param name="cancellationToken">Cancels the fetches.</param>
    /// <returns>True when the cursor sat on a node and the siblings were expanded.</returns>
    public async Task<bool> ExpandSiblingsOfCursorAsync(CancellationToken cancellationToken)
    {
        if (Cursor is not { } key)
        {
            return false;
        }

        await ExpandAllSiblingsAsync(key, cancellationToken).ConfigureAwait(true);
        return true;
    }

    private void MoveCursorToEdge(bool last)
    {
        var rows = Rows;
        if (rows.Count == 0)
        {
            Cursor = null;
            return;
        }

        Cursor = last ? rows[^1].Key : rows[0].Key;
        RaiseChanged();
    }

    private void JumpTypeAhead(string prefix)
    {
        var rows = Rows;
        if (rows.Count == 0)
        {
            return;
        }

        var start = Math.Max(0, CursorIndex);
        for (var offset = 0; offset < rows.Count; offset++)
        {
            var row = rows[(start + offset) % rows.Count];
            if (row.Node.IsLocked)
            {
                continue;
            }

            if (row.Node.Title.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                Cursor = row.Key;
                RaiseChanged();
                return;
            }
        }
    }
}
