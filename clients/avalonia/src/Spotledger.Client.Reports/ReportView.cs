using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;

namespace Spotledger.Client.Reports;

/// <summary>
/// The report screen: toolbar, the parameter form built from the description with the field controls, the
/// honesty banners, the read-only grid and a status line. It holds no rule; what it shows and enables comes from
/// the <see cref="ReportViewModel"/>. All server text is shown as plain text, never as markup.
/// <para>Keys: Enter in the form runs the report, Ctrl+Enter runs it from anywhere in the tab, Escape cancels a
/// run in flight, Ctrl+E exports, Ctrl+C in the grid copies the selection as TSV.</para>
/// </summary>
public sealed class ReportView : UserControl, IDisposable
{
    private readonly ReportViewModel _model;
    private readonly Func<string, ILookupSource?>? _lookups;
    private readonly SpotGrid _grid = new() { SelectionMode = GridSelectionMode.Extended, PrefetchMargin = 50 };
    private readonly WrapPanel _form = new() { Orientation = Orientation.Horizontal };
    private readonly StackPanel _toolbar = new() { Orientation = Orientation.Horizontal, Spacing = Metric("Space.2", 8) };
    private readonly SpotButton _run = new() { Content = "Run" };
    private readonly SpotButton _cancel = new() { Content = "Cancel", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _export = new() { Content = "Export CSV", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _reset = new() { Content = "Reset", Variant = SpotButtonVariant.Secondary };
    private readonly SpotButton _saved = new() { Content = "Saved parameters", Variant = SpotButtonVariant.Secondary };
    private readonly SpotTextInput _filter = new() { Placeholder = "Filter rows", ShowClearButton = true, MinWidth = 200 };
    private readonly TextBlock _message = new() { TextWrapping = TextWrapping.Wrap, IsVisible = false };
    private readonly TextBlock _truncation = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Border _truncationBox = new() { IsVisible = false, BorderThickness = new Thickness(3, 0, 0, 0), Padding = new Thickness(8, 4) };
    private readonly TextBlock _removed = new() { TextWrapping = TextWrapping.Wrap, IsVisible = false, Opacity = 0.85 };
    private readonly TextBlock _notes = new() { TextWrapping = TextWrapping.Wrap, IsVisible = false, Opacity = 0.85 };
    private readonly TextBlock _status = new() { Opacity = 0.8 };
    private readonly TextBlock _empty = new() { HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap, IsVisible = false };
    private readonly List<(ReportParameter Parameter, Control Control, SpotField Field)> _fields = [];
    private readonly List<IDisposable> _subscriptions = [];
    private ReportResultSource? _shownSource;
    private bool _disposed;

    /// <summary>Creates the view over a (loading or loaded) report. <paramref name="lookups"/> builds the lookup source
    /// for a link parameter from the target DocType (null: a link takes its id as text).</summary>
    public ReportView(ReportViewModel model, Func<string, ILookupSource?>? lookups = null)
    {
        _model = model ?? throw new ArgumentNullException(nameof(model));
        _lookups = lookups;
        Focusable = true;
        AutomationProperties.SetName(this, model.Title);
        AutomationProperties.SetName(_grid, model.Title + " result");
        AutomationProperties.SetName(_filter, "Filter rows");
        AutomationProperties.SetName(_status, "Status");
        AutomationProperties.SetName(_truncation, "Truncation notice");

        foreach (var control in new Control[] { _run, _cancel, _reset, _saved, _export, _filter })
        {
            _toolbar.Children.Add(control);
        }

        _truncationBox.Child = _truncation;
        var root = new DockPanel { Margin = new Thickness(Metric("Space.4", 16)) };
        foreach (var (control, dock) in new (Control, Dock)[] { (_toolbar, Dock.Top), (_form, Dock.Top), (_message, Dock.Top), (_truncationBox, Dock.Top), (_removed, Dock.Top), (_notes, Dock.Top), (_status, Dock.Bottom) })
        {
            DockPanel.SetDock(control, dock);
            root.Children.Add(control);
        }

        root.Children.Add(new Panel { Children = { _grid, _empty } });
        Content = root;

        Watch("Color.Text.Primary", brush => Foreground = brush);
        Watch("Color.Surface.Canvas", brush => Background = brush);
        Watch("Color.Semantic.Warning", brush => _truncationBox.BorderBrush = brush);
        Watch("Color.Surface.Sunken", brush => _truncationBox.Background = brush);
        Watch("Color.Semantic.Danger", brush => _message.Foreground = brush);

        _run.Click += async (_, _) => await RunAsync().ConfigureAwait(true);
        _cancel.Click += (_, _) => _model.CancelRun();
        _reset.Click += (_, _) => _model.ResetParameters();
        _export.Click += async (_, _) => await _model.ExportCsvAsync().ConfigureAwait(true);
        _saved.Click += (_, _) => ShowSavedMenu();
        _filter.PropertyChanged += (_, e) =>
        {
            if (e.Property == SpotTextInput.TextProperty)
            {
                _model.SetQuickFilter(_filter.Text);
            }
        };
        _grid.ColumnReordered += (_, e) => _shownSource?.MoveColumn(e.FromIndex, e.ToIndex);
        _grid.GridKeyDown += OnGridKey;
        AddHandler(KeyDownEvent, OnShortcut, Avalonia.Interactivity.RoutingStrategies.Tunnel);
        _model.StateChanged += OnStateChanged;
        _model.ParametersChanged += OnParametersChanged;
        _model.ResultChanged += OnResultChanged;
        BuildForm();
        OnResultChanged(null, EventArgs.Empty);
        Refresh();
    }

    /// <summary>The view model.</summary>
    public ReportViewModel Model => _model;

    /// <summary>The result grid, for tests and the host's focus handling.</summary>
    public SpotGrid Grid => _grid;

    /// <summary>The parameter form's field controls by parameter name.</summary>
    public IReadOnlyDictionary<string, Control> ParameterControls => _fields.ToDictionary(f => f.Parameter.Name, f => f.Control, StringComparer.Ordinal);

    /// <summary>The form panel.</summary>
    public WrapPanel Form => _form;

    /// <summary>The toolbar.</summary>
    public StackPanel Toolbar => _toolbar;

    /// <summary>The Run button.</summary>
    public SpotButton RunButton => _run;

    /// <summary>The truncation banner text ("" when hidden).</summary>
    public string TruncationText => _truncationBox.IsVisible ? _truncation.Text ?? string.Empty : string.Empty;

    /// <summary>The removed-columns note ("" when hidden).</summary>
    public string RemovedColumnsText => _removed.IsVisible ? _removed.Text ?? string.Empty : string.Empty;

    /// <summary>The message line ("" when hidden).</summary>
    public string MessageText => _message.IsVisible ? _message.Text ?? string.Empty : string.Empty;

    /// <summary>The state text shown in place of the grid ("" when the grid shows).</summary>
    public string StateText => _empty.IsVisible ? _empty.Text ?? string.Empty : string.Empty;

    /// <summary>The status line text.</summary>
    public string StatusLine => _status.Text ?? string.Empty;

    /// <summary>Commits the form's controls, then runs the report.</summary>
    public Task RunAsync()
    {
        CommitControls();
        return _model.RunAsync();
    }

    /// <summary>Commits what the controls hold into the parameter states (call before reading or running).</summary>
    public void CommitControls()
    {
        foreach (var (parameter, control, _) in _fields)
        {
            FieldControlFactory.GetBinding(control).CommitTo(parameter.State);
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
        foreach (var subscription in _subscriptions)
        {
            subscription.Dispose();
        }

        _model.StateChanged -= OnStateChanged;
        _model.ParametersChanged -= OnParametersChanged;
        _model.ResultChanged -= OnResultChanged;
        if (_shownSource is not null)
        {
            _shownSource.Changed -= OnSourceChanged;
        }

        _grid.Source = null;
    }

    private void BuildForm()
    {
        _form.Children.Clear();
        _fields.Clear();
        if (_model.Parameters is not { } parameters)
        {
            return;
        }

        foreach (var parameter in parameters.Items)
        {
            var control = FieldControlFactory.Create(parameter.Field, parameter.State);
            control.MinWidth = 200;
            if (control is SpotLookupBox box && parameter.Field.LinkTo is { } target)
            {
                box.Source = _lookups?.Invoke(target);
            }

            var field = new SpotField { Label = parameter.Def.Label, Content = control, IsRequired = parameter.Def.Required, Margin = new Thickness(0, 0, Metric("Space.4", 16), Metric("Space.2", 8)) };
            _form.Children.Add(field);
            _fields.Add((parameter, control, field));
        }

        // Enter in a single-line field runs the report; a multi-line field keeps Enter.
        _form.AddHandler(KeyDownEvent, OnFormKey, Avalonia.Interactivity.RoutingStrategies.Bubble);
    }

    private void OnFormKey(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter && e.KeyModifiers == KeyModifiers.None && e.Source is not SpotTextArea && !e.Handled)
        {
            e.Handled = true;
            _ = RunAsync();
        }
    }

    private void OnShortcut(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.Enter && e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            e.Handled = true;
            _ = RunAsync();
        }
        else if (e.Key == Key.Escape && _model.Phase == ReportPhase.Running)
        {
            e.Handled = true;
            _model.CancelRun();
        }
        else if (e.Key == Key.E && e.KeyModifiers.HasFlag(KeyModifiers.Control) && _model.CanExport)
        {
            e.Handled = true;
            _ = _model.ExportCsvAsync();
        }
    }

    private void OnGridKey(object? sender, GridKeyEventArgs e)
    {
        if (e.IsEditing)
        {
            return;
        }

        if (e.Key == Key.C && e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            e.Handled = true;
            _ = _model.CopySelectionAsync(_grid.SelectedRows);
        }
        else if (e.Key == Key.Enter && e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            e.Handled = true;
            _ = RunAsync();
        }
    }

    private void OnStateChanged(object? sender, EventArgs e) => Refresh();

    private void OnParametersChanged(object? sender, EventArgs e)
    {
        if (_fields.Count == 0 && _model.Parameters is { Items.Count: > 0 })
        {
            BuildForm();
        }
        else
        {
            foreach (var (parameter, control, field) in _fields)
            {
                FieldControlFactory.GetBinding(control).Load(parameter.State);
                field.ErrorText = parameter.Error;
            }
        }

        Refresh();
    }

    private void OnResultChanged(object? sender, EventArgs e)
    {
        if (_shownSource is not null)
        {
            _shownSource.Changed -= OnSourceChanged;
        }

        _shownSource = _model.Source;
        if (_shownSource is null)
        {
            _grid.Source = null;
            _grid.Footer = null;
            return;
        }

        _shownSource.Changed += OnSourceChanged;
        _grid.Columns = _shownSource.BuildGridColumns();
        _grid.Source = _shownSource;
        _grid.Footer = _shownSource.BuildFooter();
        Refresh();
    }

    private void OnSourceChanged(object? sender, GridSourceChangedEventArgs e)
    {
        if (_shownSource is not null)
        {
            _grid.Footer = _shownSource.BuildFooter();
        }

        _status.Text = _model.StatusText;
    }

    private void Refresh()
    {
        var phase = _model.Phase;
        _status.Text = _model.StatusText;
        _run.IsEnabled = _model.CanRun;
        _run.IsBusy = phase == ReportPhase.Running;
        _cancel.IsEnabled = phase == ReportPhase.Running;
        _cancel.IsVisible = phase == ReportPhase.Running;
        _export.IsEnabled = _model.CanExport;
        _reset.IsEnabled = phase == ReportPhase.Ready && _fields.Count > 0;
        _saved.IsEnabled = phase == ReportPhase.Ready && _fields.Count > 0;
        _filter.IsEnabled = _model.Source is not null;
        _form.IsEnabled = phase != ReportPhase.Running;
        _form.IsVisible = _fields.Count > 0;
        foreach (var (parameter, _, field) in _fields)
        {
            field.ErrorText = parameter.Error;
        }

        _message.Text = _model.Message;
        _message.IsVisible = _model.Message is not null && phase != ReportPhase.Failed;
        _truncation.Text = BuildTruncationText();
        _truncationBox.IsVisible = _model.TruncationBanner is not null;
        _removed.Text = _model.RemovedColumnsNote;
        _removed.IsVisible = _model.RemovedColumnsNote is not null;
        _notes.Text = string.Join(" ", _model.Notes);
        _notes.IsVisible = _model.Notes.Count > 0;
        var hasGrid = _model.Source is not null;
        _grid.IsVisible = hasGrid;
        _empty.IsVisible = !hasGrid && (phase is ReportPhase.Failed or ReportPhase.Loading or ReportPhase.Running);
        _empty.Text = phase switch
        {
            ReportPhase.Failed => _model.Message ?? "The report cannot be shown.",
            ReportPhase.Loading => "Loading the report…",
            ReportPhase.Running => "Running…",
            _ => null,
        };
    }

    private string? BuildTruncationText()
    {
        if (_model.TruncationBanner is not { } banner)
        {
            return null;
        }

        var narrowing = _model.NarrowingParameters;
        return narrowing.Count == 0 ? banner : banner + " You can narrow it with: " + string.Join(", ", narrowing) + ".";
    }

    private void ShowSavedMenu()
    {
        var items = new List<Control>();
        foreach (var set in _model.ParameterSets)
        {
            var name = set.Name;
            var apply = new MenuItem { Header = name };
            apply.Click += (_, _) => _model.ApplyParameterSet(name);
            items.Add(apply);
        }

        var save = new MenuItem { Header = "Save these parameters…" };
        save.Click += async (_, _) => await _model.SaveParameterSetAsync().ConfigureAwait(true);
        if (items.Count > 0)
        {
            items.Add(new Separator());
            var remove = new MenuItem
            {
                Header = "Delete a saved set",
                ItemsSource = _model.ParameterSets.Select(s =>
                {
                    var item = new MenuItem { Header = s.Name };
                    item.Click += async (_, _) => await _model.DeleteParameterSetAsync(s.Name).ConfigureAwait(true);
                    return item;
                }).ToArray(),
            };
            items.Add(remove);
        }

        items.Add(save);
        _saved.Flyout = new MenuFlyout { ItemsSource = items };
        _saved.Flyout.ShowAt(_saved);
    }

    private void Watch(string token, Action<IBrush?> apply)
        => _subscriptions.Add(this.GetResourceObservable(token).Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
            apply(value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null }))));

    private static double Metric(string token, double fallback)
        => Application.Current is { } app && app.TryGetResource(token, Avalonia.Styling.ThemeVariant.Light, out var value) && value is double d
            ? d
            : fallback;
}
