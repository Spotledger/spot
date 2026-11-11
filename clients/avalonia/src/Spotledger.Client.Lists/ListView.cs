using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

/// <summary>
/// The list screen: toolbar, quick search, filter chips and builder, the virtualized grid, a status line.
/// It holds no rule: what it shows and enables comes from the <see cref="ListViewModel"/>.
/// <para>Keys: Enter or double-click opens the row, Ctrl+N creates, F5 refreshes, Delete deletes the
/// selection (after a confirmation). On a chip, Delete or Backspace removes it.</para>
/// All server text is shown as plain text, never as markup.
/// </summary>
public sealed partial class ListView : UserControl, IDisposable
{
    private readonly ListViewModel _model;
    private readonly Func<string, string, ILookupSource?>? _lookups;
    private readonly SpotGrid _grid = new() { SelectionMode = GridSelectionMode.Extended };
    private readonly StackPanel _toolbar = new() { Orientation = Orientation.Horizontal, Spacing = Metric("Space.2", 8) };
    private readonly SpotTextInput _search = new() { Placeholder = "Search", ShowClearButton = true, MinWidth = 220 };
    private readonly WrapPanel _chips = new() { Orientation = Orientation.Horizontal };
    private readonly TextBlock _status = new() { Opacity = 0.8 };
    private readonly TextBlock _banner = new() { TextWrapping = TextWrapping.Wrap, IsVisible = false };
    private readonly TextBlock _empty = new() { HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap, IsVisible = false };
    private readonly SpotButton _new = new() { Content = "New" };
    private readonly SpotButton _refresh = new() { Content = "Refresh", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _delete = new() { Content = "Delete", Variant = SpotButtonVariant.Danger };
    private readonly SpotButton _export = new() { Content = "Export CSV", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _columns = new() { Content = "Columns", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _saved = new() { Content = "Saved filters", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _addFilter = new() { Content = "Add filter", Variant = SpotButtonVariant.Secondary };
    private readonly IDisposable _canvas;
    private readonly IDisposable _ink;
    private readonly Dictionary<int, string> _knownIds = [];
    private bool _disposed;

    /// <summary>Creates the view over a (loading or loaded) list. <paramref name="lookups"/> builds the lookup
    /// source of a link field of this DocType (null: link filters take ids as text).</summary>
    public ListView(ListViewModel model, Func<string, string, ILookupSource?>? lookups = null)
    {
        _model = model ?? throw new ArgumentNullException(nameof(model));
        _lookups = lookups;
        AutomationProperties.SetName(this, model.Title);
        AutomationProperties.SetName(_grid, model.Title + " list");
        AutomationProperties.SetName(_search, "Search");
        AutomationProperties.SetName(_status, "Status");
        Focusable = true;
        _grid.PrefetchMargin = 50;

        foreach (var button in new Control[] { _new, _refresh, _delete, _export, _columns, _saved })
        {
            _toolbar.Children.Add(button);
        }

        _toolbar.Children.Add(_search);
        var chipRow = new DockPanel { LastChildFill = true, Margin = new Thickness(0, Metric("Space.2", 8)) };
        DockPanel.SetDock(_addFilter, Dock.Left);
        chipRow.Children.Add(_addFilter);
        chipRow.Children.Add(_chips);

        var builder = BuildBuilder();
        var root = new DockPanel { Margin = new Thickness(Metric("Space.4", 16)) };
        DockPanel.SetDock(_toolbar, Dock.Top);
        DockPanel.SetDock(chipRow, Dock.Top);
        DockPanel.SetDock(builder, Dock.Top);
        DockPanel.SetDock(_banner, Dock.Top);
        DockPanel.SetDock(_status, Dock.Bottom);
        root.Children.Add(_toolbar);
        root.Children.Add(chipRow);
        root.Children.Add(builder);
        root.Children.Add(_banner);
        root.Children.Add(_status);
        root.Children.Add(new Panel { Children = { _grid, _empty } });
        Content = root;

        _ink = this.GetResourceObservable("Color.Text.Primary")
            .Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
                Foreground = value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null }));
        _canvas = this.GetResourceObservable("Color.Surface.Canvas")
            .Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
                Background = value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null }));

        _new.Click += (_, _) => _model.New();
        _refresh.Click += (_, _) => _model.Refresh();
        _delete.Click += async (_, _) => await _model.DeleteSelectedAsync().ConfigureAwait(true);
        _export.Click += async (_, _) => await _model.ExportAsync().ConfigureAwait(true);
        _search.PropertyChanged += OnSearchChanged;
        _grid.RowActivated += (_, e) => _model.Open((string)e.RowKey);
        _grid.SelectionChanged += (_, _) => PublishSelection();
        _grid.GridKeyDown += OnGridKey;
        _grid.ColumnResized += (_, e) => _ = _model.SetColumnWidthAsync(e.ColumnIndex, e.NewWidth);
        _grid.ColumnReordered += (_, e) => _ = _model.MoveColumnAsync(e.FromIndex, e.ToIndex);
        AddHandler(KeyDownEvent, OnShortcut, Avalonia.Interactivity.RoutingStrategies.Tunnel);
        ConfigureMenus();
        _model.ColumnsChanged += OnColumnsChanged;
        _model.FiltersChanged += OnFiltersChanged;
        _model.StateChanged += OnStateChanged;
        OnColumnsChanged(null, EventArgs.Empty);
        OnFiltersChanged(null, EventArgs.Empty);
        Refresh();
    }

    /// <summary>The view model.</summary>
    public ListViewModel Model => _model;

    /// <summary>The grid, for tests and the host's focus handling.</summary>
    public SpotGrid Grid => _grid;

    /// <summary>The filter chips.</summary>
    public WrapPanel Chips => _chips;

    /// <summary>The search box.</summary>
    public SpotTextInput Search => _search;

    /// <summary>The toolbar.</summary>
    public StackPanel Toolbar => _toolbar;

    /// <summary>The text of the state banner or empty state currently shown ("" when none).</summary>
    public string StateText => _empty.IsVisible ? _empty.Text ?? string.Empty : _banner.IsVisible ? _banner.Text ?? string.Empty : string.Empty;

    /// <summary>The status line text.</summary>
    public string StatusLine => _status.Text ?? string.Empty;

    /// <inheritdoc cref="IDisposable.Dispose" />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _canvas.Dispose();
        _ink.Dispose();
        _model.ColumnsChanged -= OnColumnsChanged;
        _model.FiltersChanged -= OnFiltersChanged;
        _model.StateChanged -= OnStateChanged;
        _grid.Source = null;
    }

    private void OnSearchChanged(object? sender, AvaloniaPropertyChangedEventArgs e)
    {
        if (e.Property == SpotTextInput.TextProperty && (_search.Text ?? string.Empty) != _model.SearchText)
        {
            _ = _model.SetSearchTextAsync(_search.Text ?? string.Empty);
        }
    }

    private void OnColumnsChanged(object? sender, EventArgs e)
    {
        if (_model.Columns is not { } columns)
        {
            return;
        }

        _grid.Columns = columns.GridColumns.ToList();
        if (!ReferenceEquals(_grid.Source, _model.Source))
        {
            _grid.Source = _model.Source;
        }
    }

    private void OnStateChanged(object? sender, EventArgs e) => Refresh();

    private void Refresh()
    {
        _status.Text = _model.StatusText;
        _new.IsEnabled = _model.CanCreate;
        _delete.IsEnabled = _model.CanDelete;
        _export.IsEnabled = _model.CanExport;
        var ready = _model.Phase == ListPhase.Ready;
        _refresh.IsEnabled = ready;
        _columns.IsEnabled = ready;
        _saved.IsEnabled = ready;
        _addFilter.IsEnabled = ready;
        _search.IsEnabled = ready;
        _grid.IsVisible = ready;
        _banner.IsVisible = false;
        _empty.IsVisible = false;
        if (_model.Phase is ListPhase.Denied or ListPhase.Failed)
        {
            _empty.Text = _model.Message ?? "The list could not be shown.";
            _empty.IsVisible = true;
        }
        else if (ready && _model.Source is { ExactTotal: 0 } && _model.Message is null)
        {
            _empty.Text = _model.DescribeFilter().Length > 0 ? "No documents match the filter." : "There are no documents yet.";
            _empty.IsVisible = true;
        }
        else if (ready && _model.Message is { } message)
        {
            _banner.Text = message;
            _banner.IsVisible = true;
        }
    }

    private void PublishSelection()
    {
        var ids = new List<string>();
        var next = new Dictionary<int, string>();
        foreach (var index in _grid.SelectedRows)
        {
            if (_grid.TryGetRow(index, out var row) && row.Key is string key)
            {
                next[index] = key;
            }
            else if (_knownIds.TryGetValue(index, out var known))
            {
                next[index] = known;
            }
        }

        _knownIds.Clear();
        foreach (var pair in next)
        {
            _knownIds[pair.Key] = pair.Value;
            ids.Add(pair.Value);
        }

        _model.SetSelection(ids);
    }

    private void OnGridKey(object? sender, GridKeyEventArgs e)
    {
        if (e.IsEditing)
        {
            return;
        }

        if (e.Key == Key.F5 || (e.Key == Key.N && e.KeyModifiers.HasFlag(KeyModifiers.Control)) || (e.Key == Key.Delete && e.KeyModifiers == KeyModifiers.None))
        {
            e.Handled = true;
            Dispatch(e.Key, e.KeyModifiers);
        }
    }

    private void OnShortcut(object? sender, KeyEventArgs e)
    {
        if (e.Source is Visual source && Avalonia.VisualTree.VisualExtensions.FindAncestorOfType<SpotGrid>(source, true) is not null || ReferenceEquals(e.Source, _grid))
        {
            return; // The grid reports these through GridKeyDown.
        }

        if (e.Key == Key.F5 || (e.Key == Key.N && e.KeyModifiers.HasFlag(KeyModifiers.Control)))
        {
            e.Handled = true;
            Dispatch(e.Key, e.KeyModifiers);
        }
    }

    private void Dispatch(Key key, KeyModifiers modifiers)
    {
        if (key == Key.F5)
        {
            _model.Refresh();
        }
        else if (key == Key.N && modifiers.HasFlag(KeyModifiers.Control))
        {
            _model.New();
        }
        else if (key == Key.Delete)
        {
            _ = _model.DeleteSelectedAsync();
        }
    }

    private static double Metric(string token, double fallback)
        => Application.Current is { } app && app.TryGetResource(token, Avalonia.Styling.ThemeVariant.Light, out var value) && value is double d
            ? d
            : fallback;
}
