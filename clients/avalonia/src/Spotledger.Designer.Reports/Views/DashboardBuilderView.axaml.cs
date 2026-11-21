using System.ComponentModel;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Media;
using Avalonia.VisualTree;

namespace Spotledger.Designer.Reports.Views;

/// <summary>
/// The dashboard builder's view. It draws the layout grid, moves a tile by dragging its header,
/// carries a tile kind from the gallery onto the grid, and forwards every one of those and every
/// key press to the view-model (which asks the server). While a drag is in progress it shows the
/// cells the tile would take, green when they are free and red when they cover another tile; the
/// server still decides, so a drop it refuses shows the refusal and leaves the tile where it was.
/// </summary>
public partial class DashboardBuilderView : UserControl
{
    /// <summary>The tag on the footprint over a free place.</summary>
    public const string FreeHintTag = "free";

    /// <summary>The tag on the footprint over another tile (or off the grid).</summary>
    public const string BlockedHintTag = "overlap";

    /// <summary>The footprint's fill over a free place: the theme's success colour, at low alpha.</summary>
    public static IBrush FreeCellBrush { get; } = new SolidColorBrush(Color.FromArgb(56, 21, 128, 61));

    /// <summary>The footprint's fill over a taken place: the theme's danger colour, at low alpha.</summary>
    public static IBrush BlockedCellBrush { get; } = new SolidColorBrush(Color.FromArgb(56, 185, 28, 28));

    private const double HeaderHeight = 16;
    private const double DragThreshold = 3;

    private static readonly string[] RefreshChoices = { "Only when the viewer asks", "Each time it is opened", "On a timer (seconds)" };

    private DashboardBuilderViewModel? _watched;
    private Border? _dragging;
    private string? _draggingId;
    private Point _dragStart;
    private Place? _dragOrigin;
    private bool _dragMoved;
    private Border? _dropHint;
    private TileTemplate? _galleryTemplate;
    private Point _galleryStart;
    private bool _galleryDragging;

    /// <summary>Creates the view.</summary>
    public DashboardBuilderView()
    {
        InitializeComponent();
        GalleryList.ItemsSource = DashboardBuilderViewModel.Gallery;
        FilterType.ItemsSource = Enum.GetValues<ParamType>();
        FilterType.SelectedIndex = 0;
        RefreshBox.ItemsSource = RefreshChoices;
        RefreshBox.SelectedIndex = 0;
        DataContextChanged += (_, _) => Watch();
        AddHandler(KeyDownEvent, OnKeyDown, RoutingStrategies.Bubble);

        // A tile kind dragged out of the gallery: the button under the pointer keeps its own click,
        // so the press is watched here and only a real drag is taken over.
        AddHandler(PointerPressedEvent, OnViewPointerPressed, RoutingStrategies.Tunnel);
        AddHandler(PointerMovedEvent, OnViewPointerMoved, RoutingStrategies.Tunnel);
        AddHandler(PointerReleasedEvent, OnViewPointerReleased, RoutingStrategies.Tunnel);
    }

    private DashboardBuilderViewModel? Builder => DataContext as DashboardBuilderViewModel;

    private void Watch()
    {
        if (_watched is not null)
        {
            _watched.PropertyChanged -= OnBuilderChanged;
        }

        _watched = Builder;
        if (_watched is not null)
        {
            _watched.PropertyChanged += OnBuilderChanged;
        }

        Rebuild();
    }

    private void OnBuilderChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(DashboardBuilderViewModel.Draft) or nameof(DashboardBuilderViewModel.SelectedTileId) or nameof(DashboardBuilderViewModel.SelectedTile))
        {
            Rebuild();
        }
    }

    /// <summary>Draws every tile of the draft on the grid; keeps keyboard focus on the selected tile.</summary>
    private void Rebuild()
    {
        var hadFocus = GridCanvas.Children.OfType<Border>().Any(b => b.IsFocused);
        GridCanvas.Children.Clear();
        _dragging = null;
        _draggingId = null;
        _dragOrigin = null;
        _dragMoved = false;
        _dropHint = null;
        if (Builder?.Draft is not { } draft)
        {
            GridCanvas.Width = 0;
            GridCanvas.Height = 0;
            return;
        }

        GridCanvas.Width = draft.GridColumns * GridGeometry.CellWidth;
        GridCanvas.Height = draft.GridRows * GridGeometry.CellHeight;
        foreach (var tile in draft.Tiles)
        {
            var selected = tile.Id == Builder.SelectedTileId;

            // The tile is dragged by its header, so the body stays free for the tile's own controls.
            var header = new Border
            {
                Tag = tile.Id,
                Name = "TileHeader",
                Height = HeaderHeight,
                Background = Brushes.Transparent,
                Child = new TextBlock
                {
                    Text = tile.Title,
                    FontSize = 10,
                    Margin = new Thickness(4, 0),
                    VerticalAlignment = Avalonia.Layout.VerticalAlignment.Center,
                    TextTrimming = TextTrimming.CharacterEllipsis,
                },
            };
            header.Classes.Add("tile-header");
            header.PointerPressed += OnHeaderPressed;
            header.PointerMoved += OnHeaderMoved;
            header.PointerReleased += OnHeaderReleased;

            var border = new Border
            {
                Tag = tile.Id,
                Width = (tile.Place.Width * GridGeometry.CellWidth) - 2,
                Height = (tile.Place.Height * GridGeometry.CellHeight) - 2,
                BorderThickness = new Thickness(selected ? 3 : 1),
                Background = Brushes.Transparent,
                Focusable = true,
                Child = new StackPanel
                {
                    Children =
                    {
                        header,
                        new TextBlock
                        {
                            Text = $"{tile.Kind}",
                            FontSize = 10,
                            Margin = new Thickness(4, 2),
                            TextTrimming = TextTrimming.CharacterEllipsis,
                        },
                    },
                },
            };
            border.Classes.Add("tile");
            Canvas.SetLeft(border, (tile.Place.Column - 1) * GridGeometry.CellWidth);
            Canvas.SetTop(border, (tile.Place.Row - 1) * GridGeometry.CellHeight);
            border.PointerPressed += OnTilePressed;
            border.GotFocus += OnTileFocused;
            AutomationProperties.SetName(border, $"Tile {tile.Title}, row {tile.Place.Row}, column {tile.Place.Column}, {tile.Place.Width} by {tile.Place.Height}");
            GridCanvas.Children.Add(border);
            if (selected && hadFocus)
            {
                border.Focus();
            }
        }
    }

    private void OnTileFocused(object? sender, FocusChangedEventArgs e)
    {
        if (sender is Border { Tag: string id } && Builder is { } builder && builder.SelectedTileId != id)
        {
            builder.SelectedTileId = id;
        }
    }

    private void OnTilePressed(object? sender, PointerPressedEventArgs e)
    {
        if (sender is Border { Tag: string id } tile && Builder is { } builder)
        {
            builder.SelectedTileId = id;
            var current = GridCanvas.Children.OfType<Border>().FirstOrDefault(b => Equals(b.Tag, id) && b.Name != "DropHint") ?? tile;
            current.Focus();
            e.Handled = true;
        }
    }

    // ---------------------------------------------------------------- moving a tile by pointer

    private void OnHeaderPressed(object? sender, PointerPressedEventArgs e)
    {
        if (Builder is not { } builder || sender is not Border { Tag: string id } || !e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)
        {
            return;
        }

        // Selecting another tile redraws the grid, so the header that was pressed is replaced; the
        // drag carries on with the tile's new header, found by id.
        builder.SelectedTileId = id;
        var tile = GridCanvas.Children.OfType<Border>()
            .FirstOrDefault(b => Equals(b.Tag, id) && b.Name != "DropHint");
        if (tile?.Child is not StackPanel panel || panel.Children.Count == 0 || panel.Children[0] is not Border header)
        {
            return;
        }

        _dragging = tile;
        _draggingId = id;
        _dragOrigin = builder.SelectedTile?.Place;
        _dragStart = e.GetPosition(GridCanvas);
        _dragMoved = false;
        e.Pointer.Capture(header);
        tile.Focus();
        e.Handled = true;
    }

    private void OnHeaderMoved(object? sender, PointerEventArgs e)
    {
        if (_dragging is not { } border || _dragOrigin is not { } origin || Builder is not { } builder)
        {
            return;
        }

        var now = e.GetPosition(GridCanvas);
        var deltaX = now.X - _dragStart.X;
        var deltaY = now.Y - _dragStart.Y;
        if (!_dragMoved && Math.Abs(deltaX) < DragThreshold && Math.Abs(deltaY) < DragThreshold)
        {
            return;
        }

        _dragMoved = true;
        Canvas.SetLeft(border, ((origin.Column - 1) * GridGeometry.CellWidth) + deltaX);
        Canvas.SetTop(border, ((origin.Row - 1) * GridGeometry.CellHeight) + deltaY);
        ShowHint(builder, GridGeometry.DragTarget(origin, deltaX, deltaY), _draggingId);
    }

    private async void OnHeaderReleased(object? sender, PointerReleasedEventArgs e)
    {
        e.Pointer.Capture(null);
        if (_dragging is null || _draggingId is not { } id || _dragOrigin is not { } origin || Builder is not { } builder)
        {
            EndDrag(builder: null);
            return;
        }

        var moved = _dragMoved;
        var target = GridGeometry.DragTarget(origin, e.GetPosition(GridCanvas).X - _dragStart.X, e.GetPosition(GridCanvas).Y - _dragStart.Y);
        e.Handled = true;
        EndDrag(builder);
        if (!moved || target == origin)
        {
            Rebuild();
            return;
        }

        // The server decides: a place off the grid or over another tile comes back as a refusal and
        // the grid is drawn from the draft the server still holds.
        if (!await builder.TryAsync(() => builder.MoveTileAsync(id, target)))
        {
            Rebuild();
        }
    }

    /// <summary>Forgets the drag and takes the footprint off the grid.</summary>
    private void EndDrag(DashboardBuilderViewModel? builder)
    {
        _dragging = null;
        _draggingId = null;
        _dragOrigin = null;
        _dragMoved = false;
        RemoveHint();
        builder?.ClearDropTarget();
    }

    // ---------------------------------------------------------------- dragging from the gallery

    private void OnViewPointerPressed(object? sender, PointerPressedEventArgs e)
    {
        if (Builder is null || !e.GetCurrentPoint(this).Properties.IsLeftButtonPressed || e.Source is not Visual source)
        {
            return;
        }

        var item = GalleryItem(source);
        if (item?.DataContext is not TileTemplate template)
        {
            return;
        }

        _galleryTemplate = template;
        _galleryStart = e.GetPosition(GridCanvas);
        _galleryDragging = false;
    }

    private void OnViewPointerMoved(object? sender, PointerEventArgs e)
    {
        if (_galleryTemplate is not { } template || Builder is not { } builder)
        {
            return;
        }

        var now = e.GetPosition(GridCanvas);
        if (!_galleryDragging && Math.Abs(now.X - _galleryStart.X) < DragThreshold && Math.Abs(now.Y - _galleryStart.Y) < DragThreshold)
        {
            return;
        }

        _galleryDragging = true;
        if (GridGeometry.Within(now.X, now.Y, builder.GridColumns, builder.GridRows))
        {
            ShowHint(builder, GridGeometry.CellAt(now.X, now.Y, template.Width, template.Height), movingTileId: null);
        }
        else
        {
            RemoveHint();
            builder.ClearDropTarget();
        }
    }

    private async void OnViewPointerReleased(object? sender, PointerReleasedEventArgs e)
    {
        if (_galleryTemplate is not { } template)
        {
            return;
        }

        var dragged = _galleryDragging;
        var builder = Builder;
        var now = e.GetPosition(GridCanvas);
        _galleryTemplate = null;
        _galleryDragging = false;
        if (!dragged)
        {
            // A plain click on a gallery button: the button's own command adds the tile at the
            // first free place.
            return;
        }

        // A drag is not a click: the button must not also add a tile at the first free place.
        e.Handled = true;
        e.Pointer.Capture(null);
        RemoveHint();
        builder?.ClearDropTarget();
        if (builder is null || !GridGeometry.Within(now.X, now.Y, builder.GridColumns, builder.GridRows))
        {
            return;
        }

        var place = GridGeometry.CellAt(now.X, now.Y, template.Width, template.Height);
        await builder.TryAsync(() => builder.AddFromGalleryAtAsync(template, place));
    }

    /// <summary>The control of the gallery item a pointer landed in, or null when it is not one.</summary>
    private Control? GalleryItem(Visual source)
    {
        Control? item = null;
        for (Visual? visual = source; visual is not null; visual = visual.GetVisualParent())
        {
            if (visual is Control control && control.DataContext is TileTemplate)
            {
                item = control;
            }

            if (ReferenceEquals(visual, GalleryList))
            {
                break;
            }
        }

        return item;
    }

    // ---------------------------------------------------------------- the drop footprint

    private void ShowHint(DashboardBuilderViewModel builder, Place place, string? movingTileId)
    {
        builder.ShowDropTarget(place, movingTileId);
        if (_dropHint is null)
        {
            _dropHint = new Border
            {
                Name = "DropHint",
                IsHitTestVisible = false,
                BorderThickness = new Thickness(2),
            };
            GridCanvas.Children.Add(_dropHint);
        }

        var free = builder.DropTargetIsFree;
        var brush = free ? FreeCellBrush : BlockedCellBrush;
        _dropHint.Tag = free ? FreeHintTag : BlockedHintTag;
        _dropHint.Background = brush;
        _dropHint.BorderBrush = brush;
        _dropHint.Width = (place.Width * GridGeometry.CellWidth) - 2;
        _dropHint.Height = (place.Height * GridGeometry.CellHeight) - 2;
        Canvas.SetLeft(_dropHint, (place.Column - 1) * GridGeometry.CellWidth);
        Canvas.SetTop(_dropHint, (place.Row - 1) * GridGeometry.CellHeight);
        AutomationProperties.SetName(
            _dropHint,
            free
                ? $"Free at row {place.Row}, column {place.Column}"
                : $"Taken at row {place.Row}, column {place.Column}");
    }

    private void RemoveHint()
    {
        if (_dropHint is not null)
        {
            GridCanvas.Children.Remove(_dropHint);
            _dropHint = null;
        }
    }

    // ---------------------------------------------------------------- keyboard

    private async void OnKeyDown(object? sender, KeyEventArgs e)
    {
        if (Builder is not { } builder)
        {
            return;
        }

        // Escape abandons a drag: the tile stays where the server has it and nothing is sent.
        if (e.Key == Key.Escape && _dragging is not null && _dragMoved)
        {
            e.Handled = true;
            EndDrag(builder);
            Rebuild();
            return;
        }

        if (e.Key == Key.Delete && builder.SelectedTile is not null && GridCanvas.Children.Any(c => c.IsFocused))
        {
            e.Handled = true;
            await builder.TryAsync(builder.RemoveSelectedAsync);
            return;
        }

        if ((e.KeyModifiers & KeyModifiers.Alt) == 0)
        {
            return;
        }

        var resize = (e.KeyModifiers & KeyModifiers.Shift) != 0;
        var (rows, columns) = e.Key switch
        {
            Key.Up => (-1, 0),
            Key.Down => (1, 0),
            Key.Left => (0, -1),
            Key.Right => (0, 1),
            _ => (0, 0),
        };
        if (rows == 0 && columns == 0)
        {
            return;
        }

        e.Handled = true;
        await builder.TryAsync(() => resize ? builder.ResizeSelectedAsync(columns, rows) : builder.MoveSelectedAsync(rows, columns));
    }

    private Task Run(Func<DashboardBuilderViewModel, Task<bool>> work) =>
        Builder is { } builder ? builder.TryAsync(() => work(builder)) : Task.CompletedTask;

    private async void OnUp(object? sender, RoutedEventArgs e) => await Run(b => b.MoveSelectedAsync(-1, 0));

    private async void OnDown(object? sender, RoutedEventArgs e) => await Run(b => b.MoveSelectedAsync(1, 0));

    private async void OnLeft(object? sender, RoutedEventArgs e) => await Run(b => b.MoveSelectedAsync(0, -1));

    private async void OnRight(object? sender, RoutedEventArgs e) => await Run(b => b.MoveSelectedAsync(0, 1));

    private async void OnWider(object? sender, RoutedEventArgs e) => await Run(b => b.ResizeSelectedAsync(1, 0));

    private async void OnNarrower(object? sender, RoutedEventArgs e) => await Run(b => b.ResizeSelectedAsync(-1, 0));

    private async void OnTaller(object? sender, RoutedEventArgs e) => await Run(b => b.ResizeSelectedAsync(0, 1));

    private async void OnShorter(object? sender, RoutedEventArgs e) => await Run(b => b.ResizeSelectedAsync(0, -1));

    private async void OnAddFilter(object? sender, RoutedEventArgs e)
    {
        var name = (FilterName.Text ?? string.Empty).Trim();
        if (name.Length == 0 || FilterType.SelectedItem is not ParamType type)
        {
            if (Builder is { } b)
            {
                b.ReportProblem("Give the shared filter a name.");
            }

            return;
        }

        var label = string.IsNullOrWhiteSpace(FilterLabel.Text) ? name : FilterLabel.Text.Trim();
        await Run(b => b.AddSharedFilterAsync(new SharedFilter(name, label, type)));
    }

    private async void OnRemoveFilter(object? sender, RoutedEventArgs e)
    {
        if (FilterList.SelectedItem is SharedFilter filter)
        {
            await Run(b => b.RemoveSharedFilterAsync(filter.Name));
        }
    }

    private async void OnSetRefresh(object? sender, RoutedEventArgs e)
    {
        RefreshPolicy policy = RefreshBox.SelectedIndex switch
        {
            1 => new RefreshPolicy.OnOpen(),
            2 => new RefreshPolicy.Every((int)(RefreshSeconds.Value ?? 60)),
            _ => new RefreshPolicy.Manual(),
        };
        await Run(b => b.SetRefreshAsync(policy));
    }

    private async void OnShare(object? sender, RoutedEventArgs e)
    {
        var roles = (RolesBox.Text ?? string.Empty).Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries);
        await Run(b => b.ShareAsync(roles));
    }
}
