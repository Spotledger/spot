using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Spotledger.Client.Controls;

namespace Spotledger.Client.Graph;

/// <summary>
/// The virtualised hierarchy tree. One custom-drawn control: a row is a paint pass over a
/// <see cref="HierarchyRow"/>, never a control, so a 10,000-node tree keeps its cost tied to
/// the viewport. The control owns scrolling, the breadcrumb band and the input table; the
/// flattened tree, paging and keyboard model live in <see cref="HierarchyModel"/>, and
/// re-parenting lives in <see cref="HierarchyReparentController"/>. Colors and metrics come
/// from Theme tokens; indentation and chevrons mirror under right-to-left flow.
/// </summary>
public partial class SpotHierarchy : Control, IDisposable
{
    /// <summary>Identifies the <see cref="Source"/> property.</summary>
    public static readonly StyledProperty<IHierarchySource?> SourceProperty =
        AvaloniaProperty.Register<SpotHierarchy, IHierarchySource?>(nameof(Source));

    /// <summary>Identifies the <see cref="PageSize"/> property.</summary>
    public static readonly StyledProperty<int> PageSizeProperty =
        AvaloniaProperty.Register<SpotHierarchy, int>(nameof(PageSize), 100);

    /// <summary>Identifies the <see cref="ShowBreadcrumb"/> property.</summary>
    public static readonly StyledProperty<bool> ShowBreadcrumbProperty =
        AvaloniaProperty.Register<SpotHierarchy, bool>(nameof(ShowBreadcrumb), true);

    /// <summary>Identifies the <see cref="Editor"/> property.</summary>
    public static readonly StyledProperty<IHierarchyEditor?> EditorProperty =
        AvaloniaProperty.Register<SpotHierarchy, IHierarchyEditor?>(nameof(Editor));

    /// <summary>Identifies the <see cref="Undo"/> property.</summary>
    public static readonly StyledProperty<IWorkspaceUndo?> UndoProperty =
        AvaloniaProperty.Register<SpotHierarchy, IWorkspaceUndo?>(nameof(Undo));

    /// <summary>Identifies the <see cref="Navigation"/> property.</summary>
    public static readonly StyledProperty<IWorkspaceNavigation?> NavigationProperty =
        AvaloniaProperty.Register<SpotHierarchy, IWorkspaceNavigation?>(nameof(Navigation));

    internal int LastRenderedRowCount { get; private set; }

    private HierarchyModel? _model;
    private HierarchyReparentController? _reparent;
    private CancellationTokenSource _load = new();
    private bool _initialized;
    private GraphNodeKey? _dragOrigin;
    private Point _dragStart;
    private bool _dragArmed;

    /// <summary>Creates the control: focusable, clipped, with a stable automation id.</summary>
    public SpotHierarchy()
    {
        Focusable = true;
        ClipToBounds = true;
        AutomationProperties.SetAutomationId(this, "graph-hierarchy");
        AutomationProperties.SetName(this, "Spotledger hierarchy");
    }

    /// <summary>Data source. Assignment rebuilds the model and reloads the root page.</summary>
    public IHierarchySource? Source
    {
        get => GetValue(SourceProperty);
        set => SetValue(SourceProperty, value);
    }

    /// <summary>Children fetched per page.</summary>
    public int PageSize
    {
        get => GetValue(PageSizeProperty);
        set => SetValue(PageSizeProperty, value);
    }

    /// <summary>Whether the breadcrumb band of the cursor's path is drawn above the rows.</summary>
    public bool ShowBreadcrumb
    {
        get => GetValue(ShowBreadcrumbProperty);
        set => SetValue(ShowBreadcrumbProperty, value);
    }

    /// <summary>The re-parent gateway; null disables drag and cut/paste editing.</summary>
    public IHierarchyEditor? Editor
    {
        get => GetValue(EditorProperty);
        set => SetValue(EditorProperty, value);
    }

    /// <summary>The workspace undo stack; every applied re-parent registers its inverse here.</summary>
    public IWorkspaceUndo? Undo
    {
        get => GetValue(UndoProperty);
        set => SetValue(UndoProperty, value);
    }

    /// <summary>The workspace navigation seam; Enter and double-click open the node there.</summary>
    public IWorkspaceNavigation? Navigation
    {
        get => GetValue(NavigationProperty);
        set => SetValue(NavigationProperty, value);
    }

    /// <summary>The time source the type-ahead window reads.</summary>
    public IGraphClock Clock { get; set; } = SystemGraphClock.Instance;

    /// <summary>The flattened tree behind the view (null before a source is assigned).</summary>
    public HierarchyModel? Model => _model;

    /// <summary>The re-parent controller (null until an editor is assigned).</summary>
    public HierarchyReparentController? Reparent => _reparent;

    /// <summary>Raised when the cursor row is activated by Enter or a double-click.</summary>
    public event EventHandler<HierarchyRowActivatedEventArgs>? RowActivated;

    /// <summary>Raised after the selection changed (including through automation peers).</summary>
    public event EventHandler? SelectionChanged;

    /// <content>Static constructor: property invalidation.</content>
    static SpotHierarchy()
    {
        AffectsRender<SpotHierarchy>(ShowBreadcrumbProperty);
        AffectsMeasure<SpotHierarchy>(PageSizeProperty);
    }

    /// <inheritdoc />
    protected override Size MeasureOverride(Size availableSize)
    {
        var width = double.IsInfinity(availableSize.Width) ? 480 : availableSize.Width;
        var height = double.IsInfinity(availableSize.Height) ? 320 : availableSize.Height;
        return new Size(width, height);
    }

    /// <inheritdoc />
    protected override Size ArrangeOverride(Size finalSize)
    {
        SetViewport(finalSize.Width, BodyHeightFor(finalSize.Height));
        InvalidateVisual();
        return finalSize;
    }

    /// <summary>Releases the model subscription and the pending page token.</summary>
    public void Dispose()
    {
        GC.SuppressFinalize(this);
        _load.Cancel();
        _load.Dispose();
        if (_model is { } model)
        {
            model.Changed -= OnModelChanged;
            model.Dispose();
            _model = null;
        }
    }

    internal void NotifySelectionChanged() => SelectionChanged?.Invoke(this, EventArgs.Empty);

    /// <inheritdoc />
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        ArgumentNullException.ThrowIfNull(change);
        base.OnPropertyChanged(change);
        if (change.Property == SourceProperty)
        {
            RewireSource();
        }
        else if (change.Property == EditorProperty)
        {
            _reparent = Editor is { } editor ? new HierarchyReparentController(editor, Undo) : null;
        }
        else if (change.Property == UndoProperty && _reparent is { } controller)
        {
            controller.SetUndo(Undo);
        }
    }

    private void RewireSource()
    {
        _load.Cancel();
        _load.Dispose();
        _load = new CancellationTokenSource();
        if (_model is { } old)
        {
            old.Changed -= OnModelChanged;
            old.Dispose();
        }

        _initialized = false;
        _scrollRows = 0;
        _model = Source is { } source ? new HierarchyModel(source, Math.Max(1, PageSize)) : null;
        if (_model is { } model)
        {
            model.Changed += OnModelChanged;
        }

        InvalidateMeasure();
        InvalidateVisual();
        Start(EnsureInitializedAsync);
    }

    private void OnModelChanged(object? sender, EventArgs e)
    {
        InvalidateVisual();
        Start(LoadMoreForViewportAsync);
    }
}
