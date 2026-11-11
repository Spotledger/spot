using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;

namespace Spotledger.Client.Grid;

/// <summary>
/// The custom-drawn virtualized data grid. One control; no per-cell or per-row control
/// tree — every pixel is painted in <see cref="Control.Render"/> from the applied window
/// of an <see cref="IGridSource"/> (via <see cref="WindowScheduler"/>), the column model
/// and Theme tokens. Layout, scrolling, input, editing, accessibility and painting live
/// in the SpotGrid.*.cs partials.
///
/// Layout: pinned header band on top (plus a totals footer when <see cref="Footer"/> is
/// set), a leading run of frozen columns that never scroll horizontally, a scrollable
/// body. Row height comes from the density tokens (the Density.Mode attached property
/// carries the mode); every colour comes from Theme tokens in both variants.
/// </summary>
public partial class SpotGrid : Control
{
    /// <summary>Identifies the <see cref="Source"/> property.</summary>
    public static readonly StyledProperty<IGridSource?> SourceProperty =
        AvaloniaProperty.Register<SpotGrid, IGridSource?>(nameof(Source));

    /// <summary>Identifies the <see cref="Columns"/> property.</summary>
    public static readonly StyledProperty<IReadOnlyList<GridColumnModel>?> ColumnsProperty =
        AvaloniaProperty.Register<SpotGrid, IReadOnlyList<GridColumnModel>?>(nameof(Columns));

    /// <summary>Identifies the <see cref="SelectionMode"/> property.</summary>
    public static readonly StyledProperty<GridSelectionMode> SelectionModeProperty =
        AvaloniaProperty.Register<SpotGrid, GridSelectionMode>(nameof(SelectionMode), GridSelectionMode.Extended);

    /// <summary>Identifies the <see cref="Footer"/> property (a totals row drawn pinned
    /// under the body; cells are display strings the host computes).</summary>
    public static readonly StyledProperty<GridRow?> FooterProperty =
        AvaloniaProperty.Register<SpotGrid, GridRow?>(nameof(Footer));

    /// <summary>Identifies the <see cref="ClipboardSink"/> property.</summary>
    public static readonly StyledProperty<IClipboardSink?> ClipboardSinkProperty =
        AvaloniaProperty.Register<SpotGrid, IClipboardSink?>(nameof(ClipboardSink));

    /// <summary>Identifies the <see cref="EditorFactory"/> property.</summary>
    public static readonly StyledProperty<ICellEditorFactory?> EditorFactoryProperty =
        AvaloniaProperty.Register<SpotGrid, ICellEditorFactory?>(nameof(EditorFactory));

    /// <summary>Identifies the <see cref="PrefetchMargin"/> property.</summary>
    public static readonly StyledProperty<int> PrefetchMarginProperty =
        AvaloniaProperty.Register<SpotGrid, int>(nameof(PrefetchMargin), 25);

    /// <summary>Data source. Assignment rewires the window scheduler and resets state.</summary>
    public IGridSource? Source
    {
        get => GetValue(SourceProperty);
        set => SetValue(SourceProperty, value);
    }

    /// <summary>The column model (order = layout order). Mutating a column's properties
    /// (width, frozen flag…) relayouts through PropertyChanged.</summary>
    public IReadOnlyList<GridColumnModel>? Columns
    {
        get => GetValue(ColumnsProperty);
        set => SetValue(ColumnsProperty, value);
    }

    /// <summary>Row selection semantics (OneRow, Extended or Range).</summary>
    public GridSelectionMode SelectionMode
    {
        get => GetValue(SelectionModeProperty);
        set => SetValue(SelectionModeProperty, value);
    }

    /// <summary>The optional totals footer row.</summary>
    public GridRow? Footer
    {
        get => GetValue(FooterProperty);
        set => SetValue(FooterProperty, value);
    }

    /// <summary>The clipboard sink for Ctrl+C TSV copies (testable; null disables).</summary>
    public IClipboardSink? ClipboardSink
    {
        get => GetValue(ClipboardSinkProperty);
        set => SetValue(ClipboardSinkProperty, value);
    }

    /// <summary>The editor factory for the cell-editing hook (null: not editable).</summary>
    public ICellEditorFactory? EditorFactory
    {
        get => GetValue(EditorFactoryProperty);
        set => SetValue(EditorFactoryProperty, value);
    }

    /// <summary>Extra rows fetched beyond the visible view on every window request.</summary>
    public int PrefetchMargin
    {
        get => GetValue(PrefetchMarginProperty);
        set => SetValue(PrefetchMarginProperty, value);
    }

    /// <summary>Raised when Enter activates a focused row (keyboard) or a row is
    /// double-clicked (mouse).</summary>
    public event EventHandler<GridRowActivatedEventArgs>? RowActivated;

    /// <summary>Raised when a row's check state flips (check column click or Space).</summary>
    public event EventHandler<GridRowCheckedChangedEventArgs>? RowCheckedChanged;

    /// <summary>Raised after a drag resize or an auto-fit changes a column's width.</summary>
    public event EventHandler<GridColumnResizedEventArgs>? ColumnResized;

    /// <summary>Raised after a column reorder drag or its keyboard alternative.</summary>
    public event EventHandler<GridColumnReorderedEventArgs>? ColumnReordered;

    /// <summary>Raised when a header click completes a sort-cycle step; the sort is also
    /// pushed to the source via <see cref="IGridSource.Sort"/>.</summary>
    public event EventHandler<GridSortedEventArgs>? Sorted;

    /// <summary>Raised when an edit opens (host may seed the editor).</summary>
    public event EventHandler<GridEditStartedEventArgs>? EditStarted;

    /// <summary>Raised when an edit closes with commit or cancel.</summary>
    public event EventHandler<GridEditFinishedEventArgs>? EditFinished;

    /// <summary>Raised from BeginEdit so the host can veto (set Cancel).</summary>
    public event EventHandler<GridEditBeginningEventArgs>? EditBeginning;

    static SpotGrid()
    {
        AffectsRender<SpotGrid>(SourceProperty, FooterProperty, SelectionModeProperty);
        AffectsMeasure<SpotGrid>(ColumnsProperty, FooterProperty, PrefetchMarginProperty);
        FocusableProperty.OverrideDefaultValue<SpotGrid>(true);
        SourceProperty.Changed.AddClassHandler<SpotGrid>((grid, _) => grid.OnSourceChanged());
        ColumnsProperty.Changed.AddClassHandler<SpotGrid>((grid, _) => grid.OnLayoutChanged());
        FooterProperty.Changed.AddClassHandler<SpotGrid>((grid, _) => grid.OnLayoutChanged());
        PrefetchMarginProperty.Changed.AddClassHandler<SpotGrid>((grid, _) => grid.OnLayoutChanged());
        SelectionModeProperty.Changed.AddClassHandler<SpotGrid>(
            (grid, e) => grid.Selection.Kind = (GridSelectionMode)e.NewValue!);
    }

    /// <summary>Initialises the selection model (the only field requiring a constructor).</summary>
    public SpotGrid()
    {
        Selection = CreateSelection();
    }

    // ---- shared state (all other members live in the partials) ------------------
    // Field initialisers must stay in the file that declares them once; see
    // SpotGrid.Infrastructure.cs for the geometry fields and SpotGrid.Input.cs /
    // SpotGrid.Editing.cs for their interaction state.

    /// <summary>Selection/check bookkeeping keyed by row key so it survives sort/reset.</summary>
    internal GridSelectionModel Selection { get; private set; }

    /// <summary>The row under the pointer (-1 when none), for hover painting.</summary>
    internal int HoveredRow { get; set; } = -1;

    /// <summary>The focused cell (source row + column index). No value until set.
    /// Named to avoid the inherited <see cref="InputElement.Cursor"/> cursor glyph.</summary>
    internal GridCursor CellCursor
    {
        get => _cellCursor;
        set
        {
            var old = _cellCursor;
            _cellCursor = value;
            if (old.RowIndex != value.RowIndex || old.ColumnIndex != value.ColumnIndex)
            {
                CursorChanged?.Invoke(this, new GridCursorChangedEventArgs(old, value));
            }
        }
    }

    private GridCursor _cellCursor;
}