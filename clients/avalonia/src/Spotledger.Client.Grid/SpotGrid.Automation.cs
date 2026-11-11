using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Controls;

namespace Spotledger.Client.Grid;

/// <summary>Automation half of <see cref="SpotGrid"/>: the grid peer (DataGrid control
/// type, row/column counts, selection pattern) plus row and cell peers for the visible
/// window. Whole-set counts travel with the grid peer so screen readers can state
/// "row 17 of 100,000" without 100,000 peers existing.</summary>
public partial class SpotGrid
{
    /// <inheritdoc />
    protected override AutomationPeer OnCreateAutomationPeer() => new SpotGridAutomationPeer(this);

    /// <summary>True when accessibility is under test and the grid should build peers
    /// for rows outside the visible window too (small fixtures only).</summary>
    internal static bool AutomationForWholeSet { get; set; }

    /// <summary>The cached automation peer the grid's OnCreateAutomationPeer made
    /// (Avalonia caches it as well; this shim avoids the internal AutomationPeer API).</summary>
    internal SpotGridAutomationPeer? CachedPeer { get; private set; }

    /// <summary>Internal shim returning the same peer instance Avalonia would create.</summary>
    internal AutomationPeer? OnCreateAutomationPeerShim()
        => CachedPeer ??= new SpotGridAutomationPeer(this);
}

/// <summary>The grid-level peer: DataGrid type, name, row/column counts for the whole
/// set and the visible-window row peers as children.</summary>
public sealed class SpotGridAutomationPeer : ControlAutomationPeer, ISelectionProvider
{
    private readonly SpotGrid _owner;

    /// <summary>Creates the peer over the grid.</summary>
    public SpotGridAutomationPeer(SpotGrid owner) : base(owner)
    {
        _owner = owner;
    }

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.DataGrid;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(SpotGrid);

    /// <inheritdoc />
    protected override string? GetNameCore() => "Spotledger data grid";

    /// <inheritdoc />
    protected override bool IsControlElementCore() => true;

    /// <inheritdoc />
    protected override bool IsContentElementCore() => true;

    /// <summary>Rows the peer exposes: the visible window (or the whole set under test).</summary>
    internal IReadOnlyList<GridRow> PeerRows
    {
        get
        {
            var rows = _owner.AppliedRows;
            if (rows is null)
            {
                return [];
            }

            if (SpotGrid.AutomationForWholeSet)
            {
                return rows;
            }

            var first = _owner.FirstVisibleRow;
            var visible = _owner.VisibleRowCount();
            var result = new List<GridRow>(visible);
            for (var i = 0; i < rows.Count; i++)
            {
                var sourceIndex = _owner.SchedulerStart() + i;
                if (sourceIndex >= first && sourceIndex < first + visible)
                {
                    result.Add(rows[i]);
                }
            }

            return result;
        }
    }

    /// <inheritdoc />
    protected override IReadOnlyList<AutomationPeer>? GetChildrenCore()
    {
        var rows = PeerRows;
        var start = _owner.SchedulerStart();
        var peers = new List<AutomationPeer>(rows.Count);
        for (var i = 0; i < rows.Count; i++)
        {
            peers.Add(new SpotGridRowAutomationPeer(_owner, start + i, rows[i]));
        }

        return peers;
    }

    // ---- ISelectionProvider -------------------------------------------------------

    /// <inheritdoc />
    public bool CanSelectMultiple => _owner.SelectionMode != GridSelectionMode.OneRow;

    /// <inheritdoc />
    public bool IsSelectionRequired => false;

    /// <inheritdoc />
    public IReadOnlyList<AutomationPeer> GetSelection()
    {
        var peers = new List<AutomationPeer>();
        foreach (var row in _owner.Selection.SelectedRows)
        {
            if (_owner.RowAt(row) is { } gridRow)
            {
                peers.Add(new SpotGridRowAutomationPeer(_owner, row, gridRow));
            }
        }

        return peers;
    }
}

/// <summary>Peer for one row of the visible window. Exposes the row name (row index /
/// total), its cells as children and the selection-item pattern. Derived from
/// ControlAutomationPeer over the grid owner (the row itself has no control — the grid
/// paints it) so the abstract Core plumbing comes implemented over the owner control.</summary>
public sealed class SpotGridRowAutomationPeer : ControlAutomationPeer, ISelectionItemProvider
{
    private readonly SpotGrid _owner;
    private readonly int _rowIndex;
    private readonly GridRow _row;

    /// <summary>Creates the row peer.</summary>
    public SpotGridRowAutomationPeer(SpotGrid owner, int rowIndex, GridRow row) : base(owner)
    {
        _owner = owner;
        _rowIndex = rowIndex;
        _row = row;
    }

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.DataItem;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(GridRow);

    /// <inheritdoc />
    protected override string? GetNameCore()
    {
        var total = _owner.IsTotalKnown ? _owner.TotalRows : -1;
        return total > 0 ? $"row {_rowIndex + 1} of {total}" : $"row {_rowIndex + 1}";
    }

    /// <inheritdoc />
    protected override string? GetHelpTextCore() => SpotGrid.DescribeAnnotation(_row.RowAnnotation);

    /// <summary>The cells of the visible window as children.</summary>
    protected override IReadOnlyList<AutomationPeer>? GetChildrenCore()
    {
        if (_owner.Columns is not { } columns)
        {
            return null;
        }

        var peers = new List<AutomationPeer>(columns.Count);
        for (var c = 0; c < columns.Count && c < _row.Cells.Count; c++)
        {
            peers.Add(new SpotGridCellAutomationPeer(_owner, _rowIndex, c, _row.Cells[c], columns[c]));
        }

        return peers;
    }

    // ---- ISelectionItemProvider ------------------------------------------------------

    /// <inheritdoc />
    public bool IsSelected => _owner.Selection.HasFlag(_rowIndex, GridRowState.Selected);

    /// <inheritdoc />
    public void Select()
    {
        _owner.Selection.Kind = _owner.SelectionMode;
        _owner.Selection.Apply(_rowIndex, GridSelectionModel.ExtendMode.Replace);
    }

    /// <inheritdoc />
    public void AddToSelection()
    {
        _owner.Selection.Kind = _owner.SelectionMode;
        _owner.Selection.Apply(_rowIndex, GridSelectionModel.ExtendMode.Toggle);
    }

    /// <inheritdoc />
    public void RemoveFromSelection()
        => _owner.Selection.SetChecked(_rowIndex, false);

    /// <inheritdoc />
    public ISelectionProvider? SelectionContainer
    {
        get
        {
            var peer = Owner is SpotGrid grid ? grid.OnCreateAutomationPeerShim() : null;
            return peer as ISelectionProvider;
        }
    }
}

/// <summary>Peer for one cell of a visible row: name (column header + text), no children.
/// ControlAutomationPeer base over the grid: the cell has no control of its own.</summary>
public sealed class SpotGridCellAutomationPeer : ControlAutomationPeer
{
    private readonly SpotGrid _owner;
    private readonly int _rowIndex;
    private readonly int _columnIndex;
    private readonly GridCell _cell;
    private readonly GridColumnModel _column;

    /// <summary>Creates the cell peer.</summary>
    public SpotGridCellAutomationPeer(
        SpotGrid owner,
        int rowIndex,
        int columnIndex,
        GridCell cell,
        GridColumnModel column)
        : base(owner)
    {
        _owner = owner;
        _rowIndex = rowIndex;
        _columnIndex = columnIndex;
        _cell = cell;
        _column = column;
    }

    /// <inheritdoc />
    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Custom;

    /// <inheritdoc />
    protected override string GetClassNameCore() => nameof(GridCell);

    /// <inheritdoc />
    protected override string? GetNameCore()
        => string.IsNullOrEmpty(_cell.Text) ? $"{_column.Header}: (empty)" : $"{_column.Header}: {_cell.Text}";

    /// <inheritdoc />
    protected override bool IsContentElementCore() => true;

    /// <inheritdoc />
    protected override bool IsControlElementCore() => true;

    /// <summary>The annotation read as help text ("Error: message") so assistive
    /// technology announces why a cell is marked; null when the cell carries none.</summary>
    protected override string? GetHelpTextCore()
        => SpotGrid.DescribeAnnotation(_owner.AnnotationAt(_rowIndex, _columnIndex));
}