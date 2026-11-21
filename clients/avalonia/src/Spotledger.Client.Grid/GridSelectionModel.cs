namespace Spotledger.Client.Grid;

/// <summary>The state of one row: selected (highlight), checked (the check
/// column's box), and focused (the row anchor) are independent bits.</summary>
[Flags]
public enum GridRowState
{
    /// <summary>No state.</summary>
    None = 0,

    /// <summary>The row is in the selection.</summary>
    Selected = 1 << 0,

    /// <summary>The row's check box is on.</summary>
    Checked = 1 << 1,

    /// <summary>The row's check box is indeterminate (mixed selection).</summary>
    CheckMixed = 1 << 2,
}

/// <summary>
/// Row selection/check bookkeeping as an ordered key set (order = click order, so
/// Ctrl-click appends). Anchor and end of the last range are tracked for Shift-extension;
/// they are selection state, deliberately not focus (focus is the cell cursor).
/// </summary>
public sealed class GridSelectionModel
{
    private readonly Func<object, int> _rowByKey;
    private readonly Func<int, object> _keyByRow;
    private readonly Dictionary<object, GridRowState> _byKey = [];

    /// <summary>Creates the model. The row→key mappers let the model survive resets:
    /// flags are keyed by the opaque row key, never by index.</summary>
    public GridSelectionModel(Func<object, int> rowByKey, Func<int, object> keyByRow)
    {
        _rowByKey = rowByKey ?? throw new ArgumentNullException(nameof(rowByKey));
        _keyByRow = keyByRow ?? throw new ArgumentNullException(nameof(keyByRow));
    }

    /// <summary>Raised after any state change; the grid re-draws.</summary>
    public event EventHandler? Changed;

    /// <summary>How selection clicks extend when the mode is Extended.</summary>
    public enum ExtendMode
    {
        /// <summary>Click selects that row alone.</summary>
        Replace,

        /// <summary>Click toggles one row in place; Ctrl in Extended mode.</summary>
        Toggle,

        /// <summary>Click extends the range [anchor..click]; Shift in Extended mode.</summary>
        Range,
    }

    /// <summary>The number of rows currently flagged selected.</summary>
    public int SelectedCount => _byKey.Count(kv => (kv.Value & GridRowState.Selected) != 0);

    /// <summary>Source indices of selected rows, in selection (click) order. Resolved
    /// through the mapper on demand, so it follows the current sort/reorder.</summary>
    public IReadOnlyList<int> SelectedRows
        => _byKey.Where(kv => (kv.Value & GridRowState.Selected) != 0)
            .Select(kv => kv.Key)
            .Select(_rowByKey)
            .Where(i => i >= 0)
            .ToList();

/// <summary>The rows (by key) carrying each flag; tests assert against these.</summary>
    public IReadOnlyDictionary<object, GridRowState> Flags => _byKey;

    /// <summary>Row selection semantics in force.</summary>
    public GridSelectionMode Kind { get; set; } = GridSelectionMode.Extended;

    private int? _anchor;
    private int? _rangeEnd;

    /// <summary>The row a Shift extension ranges from.</summary>
    public int? Anchor => _anchor;

    /// <summary>The furthest row the last range extension reached.</summary>
    public int? RangeEnd => _rangeEnd;

    /// <summary>Applies one selection click. In Replace mode the selection becomes
    /// exactly that row; in Toggle mode the row flips; in Range mode [anchor..row]
    /// replaces the selection but keeps anchor. Returns true when something changed.
    /// Single (OneRow) mode moves the selection to the clicked row in every mode.</summary>
    public bool Apply(int row, ExtendMode mode)
    {
        if (row < 0)
        {
            return false;
        }

        var changed = false;
        if (Kind == GridSelectionMode.OneRow)
        {
            changed = SetSelectionSingle(row);
        }
        else if (mode == ExtendMode.Replace)
        {
            changed = ClearSelection();
            changed |= EnsureFlag(row, GridRowState.Selected, true);
            _anchor = row;
        }
        else if (mode == ExtendMode.Toggle)
        {
            // Ctrl in Extended mode: toggle one row in place.
            changed = EnsureFlag(row, GridRowState.Selected, !HasFlag(row, GridRowState.Selected));
            _anchor ??= row;
        }
        else
        {
            // Range mode (or Shift in Extended): [anchor..row] replaces the selection.
            var from = Math.Min(_anchor ?? row, row);
            var to = Math.Max(_anchor ?? row, row);
            changed = ClearSelection();
            for (var i = from; i <= to; i++)
            {
                changed |= EnsureFlag(i, GridRowState.Selected, true);
            }

            _anchor ??= row;
        }

        _rangeEnd = row;
        if (changed)
        {
            Changed?.Invoke(this, EventArgs.Empty);
        }

        return changed;
    }

    private bool SetSelectionSingle(int row)
    {
        var changed = ClearSelection();
        changed |= EnsureFlag(row, GridRowState.Selected, true);
        return changed;
    }

    /// <summary>The flags for a row (None when untracked).</summary>
    public GridRowState GetFlags(int row)
    {
        if (row < 0)
        {
            return GridRowState.None;
        }

        return _byKey.TryGetValue(_keyByRow(row), out var flags) ? flags : GridRowState.None;
    }

    /// <summary>True when the row carries the flag.</summary>
    public bool HasFlag(int row, GridRowState flag) => (GetFlags(row) & flag) != 0;

    /// <summary>Sets or clears the checked state of a row.</summary>
    public void SetChecked(int row, bool value)
    {
        if (row < 0)
        {
            return;
        }

        if (EnsureFlag(row, GridRowState.Checked, value))
        {
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Toggles a row's checked state.</summary>
    public void ToggleChecked(int row) => SetChecked(row, !HasFlag(row, GridRowState.Checked));

    /// <summary>Sets checked on every row of the current view when the header check box
    /// wants select-all; only rows the grid can enumerate (the source count) are covered.</summary>
    public void SetCheckedRange(int start, int count, bool value)
    {
        var changed = false;
        for (var i = start; i < start + count; i++)
        {
            changed |= EnsureFlag(i, GridRowState.Checked, value);
        }

        if (changed)
        {
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>Count of checked rows currently tracked.</summary>
    public int CheckedCount => _byKey.Count(kv => (kv.Value & GridRowState.Checked) != 0);

    /// <summary>Drops the Selected flag from every row. Checked state survives.</summary>
    public bool ClearSelection()
    {
        var changed = false;
        List<object>? toClear = null;
        foreach (var (key, flags) in _byKey)
        {
            if ((flags & GridRowState.Selected) != 0)
            {
                (toClear ??= []).Add(key);
            }
        }

        if (toClear is not null)
        {
            foreach (var key in toClear)
            {
                var next = _byKey[key] & ~GridRowState.Selected;
                if (next == GridRowState.None)
                {
                    _byKey.Remove(key);
                }
                else
                {
                    _byKey[key] = next;
                }
            }

            changed = true;
        }

        return changed;
    }

    /// <summary>Forgets every row's state (a source reset invalidated identities no longer meaningful).</summary>
    public void Reset()
    {
        if (_byKey.Count > 0)
        {
            _byKey.Clear();
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    private bool EnsureFlag(int row, GridRowState flag, bool value)
    {
        var key = _keyByRow(row);
        _byKey.TryGetValue(key, out var current);
        var next = value ? current | flag : current & ~flag;
        if (next == current)
        {
            return false;
        }

        if (next == GridRowState.None)
        {
            _byKey.Remove(key);
        }
        else
        {
            _byKey[key] = next;
        }

        return true;
    }
}