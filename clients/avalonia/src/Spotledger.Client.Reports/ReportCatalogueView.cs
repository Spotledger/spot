using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Controls;

namespace Spotledger.Client.Reports;

/// <summary>
/// The Reports workspace: a search box, a favourites switch and the reports grouped by app. Enter or a double-click
/// opens the report, Space or the star button toggles the favourite. One line says why the list is empty or
/// unreachable. All server text is shown as plain text.
/// </summary>
public sealed class ReportCatalogueView : UserControl, IDisposable
{
    private readonly ReportCatalogueViewModel _model;
    private readonly SpotTextInput _search = new() { Placeholder = "Search reports", ShowClearButton = true, MinWidth = 260 };
    private readonly SpotCheckBox _favourites = new() { Content = "Favourites only" };
    private readonly SpotButton _refresh = new() { Content = "Refresh", Variant = SpotButtonVariant.Secondary };
    private readonly ListBox _list = new() { SelectionMode = SelectionMode.Single };
    private readonly TextBlock _message = new() { HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap, IsVisible = false };
    private readonly List<IDisposable> _subscriptions = [];
    private bool _disposed;

    /// <summary>Creates the view over a (loading or loaded) catalogue.</summary>
    public ReportCatalogueView(ReportCatalogueViewModel model)
    {
        _model = model ?? throw new ArgumentNullException(nameof(model));
        Focusable = true;
        AutomationProperties.SetName(this, "Reports");
        AutomationProperties.SetName(_list, "Reports");
        AutomationProperties.SetName(_search, "Search reports");
        var toolbar = new StackPanel { Orientation = Orientation.Horizontal, Spacing = Metric("Space.2", 8), Margin = new Thickness(0, 0, 0, Metric("Space.2", 8)) };
        toolbar.Children.Add(_search);
        toolbar.Children.Add(_favourites);
        toolbar.Children.Add(_refresh);
        var root = new DockPanel { Margin = new Thickness(Metric("Space.4", 16)) };
        DockPanel.SetDock(toolbar, Dock.Top);
        root.Children.Add(toolbar);
        root.Children.Add(new Panel { Children = { _list, _message } });
        Content = root;

        _subscriptions.Add(this.GetResourceObservable("Color.Text.Primary").Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
            Foreground = value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null })));
        _subscriptions.Add(this.GetResourceObservable("Color.Surface.Canvas").Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
            Background = value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null })));

        _search.PropertyChanged += (_, e) =>
        {
            if (e.Property == SpotTextInput.TextProperty)
            {
                _model.SetSearch(_search.Text);
            }
        };
        _favourites.IsCheckedChanged += (_, _) => _model.SetOnlyFavourites(_favourites.IsChecked == true);
        _refresh.Click += async (_, _) => await _model.LoadAsync().ConfigureAwait(true);
        _list.DoubleTapped += (_, _) => OpenSelected();
        _list.KeyDown += OnListKey;
        _model.Changed += OnChanged;
        OnChanged(null, EventArgs.Empty);
    }

    /// <summary>The view model.</summary>
    public ReportCatalogueViewModel Model => _model;

    /// <summary>The list control, for tests and the host's focus handling.</summary>
    public ListBox List => _list;

    /// <summary>The search box.</summary>
    public SpotTextInput Search => _search;

    /// <summary>The one-line state text shown in place of the list ("" when the list shows).</summary>
    public string StateText => _message.IsVisible ? _message.Text ?? string.Empty : string.Empty;

    /// <summary>The rows of the list as plain text, headers included (group headers carry no star).</summary>
    public IReadOnlyList<string> RowTexts => _list.Items.OfType<ListBoxItem>().Select(i => i.Content?.ToString() ?? string.Empty).ToArray();

    /// <summary>Opens the selected report.</summary>
    public void OpenSelected()
    {
        if (_list.SelectedItem is ListBoxItem { Tag: string name })
        {
            _model.Open(name);
        }
    }

    /// <inheritdoc cref="IDisposable.Dispose" />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _model.Changed -= OnChanged;
        foreach (var subscription in _subscriptions)
        {
            subscription.Dispose();
        }
    }

    private void OnListKey(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter)
        {
            e.Handled = true;
            OpenSelected();
        }
        else if (e.Key == Key.Space && _list.SelectedItem is ListBoxItem { Tag: string name })
        {
            e.Handled = true;
            _ = _model.ToggleFavouriteAsync(name);
        }
    }

    private void OnChanged(object? sender, EventArgs e)
    {
        var items = new List<ListBoxItem>();
        foreach (var group in _model.Groups)
        {
            items.Add(new ListBoxItem { Content = group.App.Length == 0 ? "Other" : group.App, IsEnabled = false, FontWeight = FontWeight.SemiBold });
            foreach (var entry in group.Entries)
            {
                var text = (entry.IsFavourite ? "★ " : "   ") + entry.Summary.Label;
                var item = new ListBoxItem { Content = text, Tag = entry.Summary.Name };
                AutomationProperties.SetName(item, entry.Summary.Label + (entry.IsFavourite ? ", favourite" : string.Empty));
                items.Add(item);
            }
        }

        _list.ItemsSource = items;
        var text2 = _model.Message;
        _message.Text = text2;
        _message.IsVisible = text2 is not null && items.Count == 0;
        _list.IsVisible = !_message.IsVisible;
    }

    private static double Metric(string token, double fallback)
        => Application.Current is { } app && app.TryGetResource(token, Avalonia.Styling.ThemeVariant.Light, out var value) && value is double d
            ? d
            : fallback;
}
