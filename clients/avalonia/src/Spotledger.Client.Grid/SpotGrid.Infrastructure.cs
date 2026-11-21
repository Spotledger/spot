using Avalonia;
using Avalonia.Media;
using Avalonia.Media.Immutable;
using Avalonia.Styling;

namespace Spotledger.Client.Grid;

/// <summary>Infrastructure half of <see cref="SpotGrid"/>: source wiring, column layout
/// math (px/star widths, frozen run), scroll clamping and token lookup. The renderer,
/// input and editing partials build on these helpers.</summary>
public partial class SpotGrid : IDisposable
{
    private WindowScheduler? _scheduler;
    private IGridSource? _wiredSource;

    /// <summary>Vertical scroll offset in rows (fractional). Set through <see cref="ScrollToRow"/>.</summary>
    internal double ScrollRows { get; private set; }

    /// <summary>Horizontal scroll offset in device-independent pixels. Set through <see cref="ScrollToX"/>.</summary>
    internal double ScrollX { get; private set; }

    /// <summary>Body viewport size from the last arrange (the renderer and hit tests read these).</summary>
    internal double ViewportWidth { get; private set; }
    internal double ViewportHeight { get; private set; }

    /// <summary>Records the arranged viewport (called from ArrangeOverride) and
    /// recomputes the column layout when the width moved.</summary>
    internal void SetViewport(double width, double height)
    {
        var relayout = Math.Abs(width - ViewportWidth) > 0.01;
        ViewportWidth = width;
        ViewportHeight = height;
        if (relayout || _columnOffsets is null)
        {
            ComputeColumnLayout(width);
        }
    }

    /// <summary>The effective density mode (attached property, inherited down the tree).</summary>
    internal Spotledger.Client.Controls.DensityMode EffectiveDensity
        => Spotledger.Client.Controls.Density.GetMode(this);

    /// <summary>True when the effective density is Compact.</summary>
    internal bool IsCompactDensity
        => EffectiveDensity == Spotledger.Client.Controls.DensityMode.Compact;

    /// <summary>The effective row height: the density token Grid.RowHeight for the mode.</summary>
    internal double RowHeight => TokenDouble(IsCompactDensity
        ? "Density.Compact.Grid.RowHeight"
        : "Density.Comfortable.Grid.RowHeight");

    /// <summary>Cell horizontal padding from the density token.</summary>
    internal double CellPadding => TokenDouble(IsCompactDensity
        ? "Density.Compact.Grid.CellPadding"
        : "Density.Comfortable.Grid.CellPadding");

    /// <summary>Header band height (one header row tall).</summary>
    internal double HeaderHeight => RowHeight;

    /// <summary>The frozen run length honoured (0 when none or uncomputed).</summary>
    internal int FrozenColumnCount => _visibleFrozenColumns < 0 ? 0 : _visibleFrozenColumns;

    /// <summary>Releases the scheduler and unwires the source.</summary>
    public void Dispose()
    {
        GC.SuppressFinalize(this);
        _scheduler?.Dispose();
        if (_wiredSource is { } source)
        {
            source.Changed -= OnSourceSetChanged;
            _wiredSource = null;
        }
    }

    /// <summary>Wires a source: builds the scheduler, resets transient state and issues
    /// the first window request (deferred until layout when nothing is arranged yet).</summary>
    private void OnSourceChanged()
    {
        _scheduler?.Dispose();
        if (_wiredSource is { } old)
        {
            old.Changed -= OnSourceSetChanged;
        }

        _scheduler = null;
        ResetSelection();
        CellCursor = new GridCursor(-1, -1, GridCursorOrigin.None);
        HoveredRow = -1;
        ScrollRows = 0;
        ScrollX = 0;
        ErrorState = null;
        SortState = (-1, GridSortDirection.None);
        _typeAhead.Clear();
        _typeAheadStamp = DateTimeOffset.MinValue;
        CloseEditor(commit: false);
        _columnOffsets = null;
        _columnWidths = null;
        _visibleFrozenColumns = -1;
        _totalColumnWidth = 0;
        SetViewport(0, 0);

        if (Source is { } source)
        {
            _scheduler = new WindowScheduler(source, PrefetchMargin);
            _scheduler.SetCount(source.Count);
            _scheduler.WindowCompleted += OnWindowCompleted;
            _scheduler.WindowFailed += OnWindowFailed;
            _scheduler.RequestIssued += OnSchedulerIssued;
            _scheduler.RequestCancelled += OnSchedulerCancelled;
            source.Changed += OnSourceSetChanged;
            _wiredSource = source;
        }

        InvalidateMeasure();
        InvalidateVisual();
    }

    /// <summary>The error message and failed window start row when a fetch failed
    /// (null when fine). Kept in the shared state block for the renderer.</summary>
    internal (string Message, int Start)? ErrorState { get; private set; }

    /// <summary>The current sort (column model index and direction), if any.</summary>
    internal (int Column, GridSortDirection Direction) SortState { get; private set; } = (-1, GridSortDirection.None);

    /// <summary>Column count, or 0 with no model.</summary>
    internal int ColumnCount => Columns?.Count ?? 0;

    private void OnSourceSetChanged(object? sender, GridSourceChangedEventArgs e)
    {
        // Selection is keyed by row key: a reset (sort) keeps flags and they re-resolve
        // against the new order; raw row insertions/removals change the identity space
        // and clear the selection.
        if (e.Kind == GridChangeKind.RowsInserted || e.Kind == GridChangeKind.RowsRemoved)
        {
            Selection.Reset();
        }

        if (Source is { } source)
        {
            _scheduler?.SetCount(source.Count);
        }

        _scheduler?.DiscardWindow();
        ErrorState = null;
        InvalidateMeasure();
        InvalidateVisual();
        RequestWindow();
    }

    private void OnWindowCompleted(object? sender, GridWindowCompletedEventArgs e)
    {
        ErrorState = null;
        InvalidateVisual();
        // A window that comes back short of its request teaches an unknown/estimated
        // count; the scrollbar extent should follow it.
        if (!_scheduler?.IsTotalKnown == true)
        {
            InvalidateMeasure();
        }
    }

    private void OnWindowFailed(object? sender, GridWindowFailedEventArgs e)
    {
        ErrorState = (e.Error.Message, e.Request.Start);
        InvalidateVisual();
    }

    /// <summary>The window scheduler feeding rows for the current source (tests read
    /// its request log and completions).</summary>
    internal WindowScheduler? Scheduler => _scheduler;

    private void OnSchedulerIssued(object? sender, GridWindowRequest e) => InvalidateVisual();

    private void OnSchedulerCancelled(object? sender, GridWindowRequest e) => InvalidateVisual();

    private void OnLayoutChanged()
    {
        _columnOffsets = null;
        _columnWidths = null;
        _visibleFrozenColumns = -1;
        _totalColumnWidth = 0;
        // Selection is keyed by row key and therefore survives column reorders and
        // relayouts; it is reset only when the source itself changes.
        InvalidateMeasure();
        InvalidateVisual();
    }

    /// <summary>Requests the current view's window from the scheduler (view rows + margin).</summary>
    internal void RequestWindow()
    {
        if (_scheduler is null)
        {
            return;
        }

        var visible = VisibleRowCount();
        if (visible <= 0)
        {
            return;
        }

        var first = FirstVisibleRow;
        _scheduler.RequestView(first, visible);
    }

    /// <summary>The first source row visible at the top of the body.</summary>
    internal int FirstVisibleRow => (int)Math.Floor(ScrollRows);

    /// <summary>Rows that fit in the body viewport (header and footer excluded).</summary>
    internal int VisibleRowCount()
    {
        var height = ViewportHeight;
        return height <= 0 ? 0 : (int)Math.Ceiling(height / RowHeight);
    }

    /// <summary>The applied window rows sliced for the viewport, or null while loading.</summary>
    internal IReadOnlyList<GridRow>? VisibleRows
        => _scheduler?.TryGetRows(FirstVisibleRow, VisibleRowCount());

    /// <summary>The applied window rows (the whole fetched slice).</summary>
    internal IReadOnlyList<GridRow>? AppliedRows => _scheduler?.LatestCompleted?.Rows;

    /// <summary>True while a fetch is outstanding.</summary>
    internal bool IsLoading => _scheduler?.HasInFlightRequest == true;

    /// <summary>Total rows laid out against (statement or learned).</summary>
    internal int TotalRows => _scheduler?.EffectiveTotalRows ?? 0;

    /// <summary>True when a total is known or learned.</summary>
    internal bool IsTotalKnown => _scheduler?.IsTotalKnown ?? false;

    /// <summary>Selection-key mapper: row key → current source index (-1 unknown).</summary>
    private int FindRowByKey(object key)
    {
        if (_scheduler?.LatestCompleted is not { } applied)
        {
            return -1;
        }

        for (var i = 0; i < applied.Rows.Count; i++)
        {
            if (Equals(applied.Rows[i].Key, key))
            {
                return applied.Start + i;
            }
        }

        return -1;
    }

    /// <summary>Selection-key mapper: source index → row key (stable synthetic when unfetched).</summary>
    private object KeyForRow(int index) => _scheduler?.LatestCompleted?.RowAt(index)?.Key ?? $"i{index}";

    /// <summary>The row at a source index from the applied window (null when unfetched).</summary>
    internal GridRow? RowAt(int index) => _scheduler?.LatestCompleted?.RowAt(index);

    /// <summary>The applied window's start index (0 when none), used by automation peers.</summary>
    internal int SchedulerStart() => _scheduler?.LatestCompleted?.Start ?? 0;

    // ---- column layout ---------------------------------------------------------

    /// <summary>Distributes widths and computes offsets. Star columns share the
    /// viewport width left after pixel columns (viewport = grid width minus the frozen
    /// run, since frozen columns never scroll but do occupy space).</summary>
    internal void ComputeColumnLayout(double viewportWidth)
    {
        var columns = Columns;
        var count = columns?.Count ?? 0;
        if (count == 0 || columns is null)
        {
            _columnOffsets = [];
            _columnWidths = [];
            _visibleFrozenColumns = 0;
            _totalColumnWidth = 0;
            return;
        }

        // Frozen honouring: only a contiguous run from index 0 counts.
        var frozen = 0;
        while (frozen < count && columns[frozen].IsFrozen)
        {
            frozen++;
        }

        _visibleFrozenColumns = frozen;

        var pixelTotal = 0d;
        var starParts = 0d;
        for (var i = 0; i < count; i++)
        {
            var width = columns[i].Width;
            if (width.IsStar)
            {
                starParts += width.Value;
            }
            else
            {
                pixelTotal += Math.Max(columns[i].MinWidth, width.Value);
            }
        }

        var starBudget = Math.Max(0, viewportWidth - pixelTotal);
        var offsets = new double[count];
        var widths = new double[count];
        var offset = 0d;
        for (var i = 0; i < count; i++)
        {
            widths[i] = DistributedWidth(columns[i], starParts, starBudget);
            offsets[i] = offset;
            offset += widths[i];
        }

        _columnOffsets = offsets;
        _columnWidths = widths;
        _totalColumnWidth = offset;

        // A frozen column that got star-distributed (star + frozen) collapses to its
        // minimum so the run never consumes the whole viewport.
        if (FrozenColumnCount > 0)
        {
            var frozenWidth = 0d;
            for (var i = 0; i < FrozenColumnCount; i++)
            {
                frozenWidth += widths[i];
            }

            if (frozenWidth > viewportWidth * 0.6 && starParts > 0)
            {
                for (var i = 0; i < FrozenColumnCount; i++)
                {
                    if (columns[i].Width.IsStar)
                    {
                        widths[i] = columns[i].MinWidth;
                    }
                }

                // Re-run the distribution once with the frozen stars collapsed.
                RedistributeAfterCollapse(viewportWidth, columns, widths, starParts);
            }
        }

        _visibleFrozenColumns = _visibleFrozenColumns < 0 ? 0 : _visibleFrozenColumns;
    }

    /// <summary>One column's distributed width: stars share the viewport budget left
    /// after the pixel columns (pro-rata by weight, floored at MinWidth), pixel columns
    /// clamp to Min/MaxWidth.</summary>
    private static double DistributedWidth(GridColumnModel column, double starParts, double starBudget)
        => column.Width.IsStar
            ? starParts > 0
                ? Math.Max(column.MinWidth, column.Width.Value / starParts * starBudget)
                : column.MinWidth
            : Math.Max(column.MinWidth, Math.Min(column.MaxWidth, column.Width.Value));

    /// <summary>Redistributes after a frozen-star collapse: the same distribution rule,
    /// with collapsed stars held at MinWidth.</summary>
    private void RedistributeAfterCollapse(
        double viewportWidth,
        IReadOnlyList<GridColumnModel> columns,
        double[] widths,
        double starParts)
    {
        var pixelTotal = 0d;
        for (var i = 0; i < columns.Count; i++)
        {
            if (!columns[i].Width.IsStar)
            {
                pixelTotal += Math.Max(columns[i].MinWidth, columns[i].Width.Value);
            }
        }

        var budget = Math.Max(0, viewportWidth - pixelTotal);
        var offset = 0d;
        for (var i = 0; i < columns.Count; i++)
        {
            var collapsed = i < FrozenColumnCount && columns[i].Width.IsStar;
            widths[i] = collapsed ? columns[i].MinWidth : DistributedWidth(columns[i], starParts, budget);
            _columnOffsets![i] = offset;
            offset += widths[i];
        }

        _totalColumnWidth = offset;
    }

    /// <summary>Column pixel offsets (entry i = column i's leading edge x) after star
    /// distribution; null until the first arrange.</summary>
    internal double[]? _columnOffsets;
    /// <summary>Column pixel widths parallel to the model (tests read these for the
    /// star-distribution assertions).</summary>
    internal double[]? _columnWidths;

    /// <summary>The applied widths (null before the first layout) — test-facing name.</summary>
    internal double[]? ColumnWidths => _columnWidths;
    /// <summary>The frozen run length actually honoured (-1 until computed).</summary>
    internal int _visibleFrozenColumns = -1;
    /// <summary>The sum of all column widths (the horizontal extent).</summary>
    internal double _totalColumnWidth;

    /// <summary>The width of the frozen run (columns that never scroll).</summary>
    internal double FrozenWidth()
    {
        if (_columnWidths is null)
        {
            return 0;
        }

        var width = 0d;
        for (var i = 0; i < FrozenColumnCount && i < _columnWidths.Length; i++)
        {
            width += _columnWidths[i];
        }

        return width;
    }

    /// <summary>The sum of every column width (the horizontal scroll extent).</summary>
    internal double TotalColumnWidth() => _totalColumnWidth;

    /// <summary>Scrolls vertically (rows) with clamping and re-requests the window.</summary>
    internal void ScrollToRow(double row)
    {
        var max = Math.Max(0, TotalRows - VisibleRowCount());
        ScrollRows = Math.Clamp(row, 0, max);
        RequestWindow();
        RepositionEditorIfOpen();
        InvalidateVisual();
    }

    /// <summary>Scrolls horizontally (px) with clamping.</summary>
    internal void ScrollToX(double x)
    {
        var max = Math.Max(0, TotalColumnWidth() - Math.Max(0, ViewportWidth - FrozenWidth()));
        ScrollX = Math.Clamp(x, 0, max);
        RepositionEditorIfOpen();
        InvalidateVisual();
    }

    /// <summary>Maximum vertical scroll in rows.</summary>
    internal double MaxScrollRows => Math.Max(0, TotalRows - VisibleRowCount());

    // ---- tokens -----------------------------------------------------------------

    /// <summary>Resolves a numeric token; falls back to compact/comfortable defaults
    /// when the app resources are not up yet (headless boot order).</summary>
    internal static double TokenDouble(string key)
    {
        if (Application.Current is { } app
            && app.TryGetResource(key, ThemeVariantService.Active, out var value)
            && value is double d)
        {
            return d;
        }

        return key.Contains("Compact", StringComparison.Ordinal) ? 32d : 40d;
    }

    /// <summary>Resolves a colour token into an immutable brush for painting.</summary>
    internal static IBrush TokenBrush(string key)
    {
        if (Application.Current is { } app
            && app.TryGetResource(key, ThemeVariantService.Active, out var value)
            && value is Color color)
        {
            return new ImmutableSolidColorBrush(color);
        }

        return Brushes.Transparent;
    }

    /// <summary>Theme-variant accessor mirroring the shell's behaviour: the app's actual
    /// variant once set, Light otherwise (the density tokens live outside the variants).</summary>
    internal static class ThemeVariantService
    {
        /// <summary>The variant brushes resolve against.</summary>
        public static ThemeVariant Active =>
            Application.Current is { } app
            && app.ActualThemeVariant is { } actual
            && !Equals(actual, ThemeVariant.Default)
                ? actual
                : ThemeVariant.Light;
    }
}