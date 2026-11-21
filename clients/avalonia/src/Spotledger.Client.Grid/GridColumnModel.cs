using System.ComponentModel;

namespace Spotledger.Client.Grid;

/// <summary>One column of a <see cref="SpotGrid"/>: header label, width (px or star),
/// frozen and check flags, sort/resize permission. Plain <c>INotifyPropertyChanged</c>
/// — setters only set their own backing field and raise for that property, so no
/// change can wipe sibling state.</summary>
public sealed class GridColumnModel : INotifyPropertyChanged
{
    private string _header = string.Empty;
    private GridColumnWidth _width = GridColumnWidth.Star(1);
    private double _minWidth = GridDefaults.MinimumColumnWidth;
    private double _maxWidth = double.PositiveInfinity;
    private bool _isFrozen;
    private bool _isCheckColumn;
    private bool _isSortable = true;
    private bool _isResizable = true;
    private GridCellAlignment _defaultAlignment = GridCellAlignment.Start;

    /// <summary>Raised when a column property changes; the grid recomputes layout.</summary>
    public event PropertyChangedEventHandler? PropertyChanged;

    /// <summary>The header label.</summary>
    public string Header
    {
        get => _header;
        set => Set(ref _header, value, nameof(Header));
    }

    /// <summary>The width (fixed pixels or star weight).</summary>
    public GridColumnWidth Width
    {
        get => _width;
        set => Set(ref _width, value, nameof(Width));
    }

    /// <summary>Resize floor; also the collapsed size when the viewport is too small.</summary>
    public double MinWidth
    {
        get => _minWidth;
        set => Set(ref _minWidth, value, nameof(MinWidth));
    }

    /// <summary>Resize ceiling.</summary>
    public double MaxWidth
    {
        get => _maxWidth;
        set => Set(ref _maxWidth, value, nameof(MaxWidth));
    }

    /// <summary>True for frozen leading columns. Only a contiguous prefix from index 0
    /// is honoured as frozen; freezing a middle column is ignored (documented
    /// defensive behaviour), so scroll geometry stays sane.</summary>
    public bool IsFrozen
    {
        get => _isFrozen;
        set => Set(ref _isFrozen, value, nameof(IsFrozen));
    }

    /// <summary>True when this column is the row check column: it shows a checkbox
    /// affordance, toggles with Space or a click, and is not part of copied TSV.</summary>
    public bool IsCheckColumn
    {
        get => _isCheckColumn;
        set => Set(ref _isCheckColumn, value, nameof(IsCheckColumn));
    }

    /// <summary>True when header clicks cycle the sort on this column.</summary>
    public bool IsSortable
    {
        get => _isSortable;
        set => Set(ref _isSortable, value, nameof(IsSortable));
    }

    /// <summary>True when the column edge can be dragged to resize.</summary>
    public bool IsResizable
    {
        get => _isResizable;
        set => Set(ref _isResizable, value, nameof(IsResizable));
    }

    /// <summary>The alignment cells use when the cell does not override it.</summary>
    public GridCellAlignment DefaultAlignment
    {
        get => _defaultAlignment;
        set => Set(ref _defaultAlignment, value, nameof(DefaultAlignment));
    }

    private void Set<T>(ref T field, T value, string propertyName)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return;
        }

        field = value;
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));
    }
}