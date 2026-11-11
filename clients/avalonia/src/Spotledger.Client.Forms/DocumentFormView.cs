using System.Collections.Specialized;
using System.ComponentModel;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.ChildTable;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms;

/// <summary>
/// The document form: header (title, state badge, toolbar), the conflict panel, the message area,
/// the resolved layout (tabs of sections, fields through <see cref="FieldControlFactory"/>, child
/// tables through <see cref="SpotChildTable"/>) and the audit footer. It holds no rule of its own:
/// everything it shows or enables comes from the <see cref="DocumentFormViewModel"/>.
/// <para>Keys: Ctrl+S save, Ctrl+Enter submit, Ctrl+N new, F6 cycles the regions (toolbar, form,
/// messages), Alt+1..9 select a tab, Esc dismisses the conflict panel.</para>
/// </summary>
public sealed class DocumentFormView : UserControl, IDisposable
{
    private readonly DocumentFormViewModel _model;
    private readonly List<FieldBinding> _bindings = [];
    private readonly List<SpotChildTable> _tables = [];
    private readonly List<(DocumentDraft Draft, EventHandler<StructureChangedEventArgs> Handler)> _tableHandlers = [];
    private readonly StackPanel _toolbar = new() { Orientation = Orientation.Horizontal, Spacing = Metric("Space.2", 8) };
    private readonly TextBlock _title = new() { FontWeight = FontWeight.SemiBold, FontSize = Metric("Font.Size.Heading", 20) };
    private readonly TextBlock _badge = new();
    private readonly Border _conflict = new() { IsVisible = false, Padding = new Thickness(Metric("Space.3", 12)) };
    private readonly ItemsControl _messages = new();
    private readonly ContentControl _body = new();
    private readonly TextBlock _audit = new() { Opacity = 0.7 };
    private readonly ContentControl _recovery = new();
    private readonly TextBlock _note = new() { Opacity = 0.7, IsVisible = false, TextWrapping = TextWrapping.Wrap };
    private readonly IDisposable _canvas;
    private readonly IDisposable _ink;
    private TabControl? _tabs;
    private int _region = -1;
    private bool _disposed;

    private sealed record FieldBinding(FieldModel Field, Control Control, FieldState State, SpotField Wrapper);

    /// <summary>Creates the view over a (loading or loaded) form.</summary>
    public DocumentFormView(DocumentFormViewModel model)
    {
        _model = model ?? throw new ArgumentNullException(nameof(model));
        AutomationProperties.SetName(this, model.Title);
        var header = new DockPanel { LastChildFill = true };
        var heading = new StackPanel { Orientation = Orientation.Horizontal, Spacing = Metric("Space.3", 12) };
        heading.Children.Add(_title);
        heading.Children.Add(_badge);
        DockPanel.SetDock(_toolbar, Dock.Right);
        header.Children.Add(_toolbar);
        header.Children.Add(heading);
        var root = new DockPanel { Margin = new Thickness(Metric("Space.4", 16)) };
        DockPanel.SetDock(header, Dock.Top);
        DockPanel.SetDock(_recovery, Dock.Top);
        DockPanel.SetDock(_note, Dock.Top);
        DockPanel.SetDock(_conflict, Dock.Top);
        DockPanel.SetDock(_messages, Dock.Top);
        DockPanel.SetDock(_audit, Dock.Bottom);
        root.Children.Add(header);
        root.Children.Add(_recovery);
        root.Children.Add(_note);
        root.Children.Add(_conflict);
        root.Children.Add(_messages);
        root.Children.Add(_audit);
        root.Children.Add(_body);
        Content = root;
        Focusable = true;
        _ink = this.GetResourceObservable("Color.Text.Primary")
            .Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
                Foreground = value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null }));
        _canvas = this.GetResourceObservable("Color.Surface.Canvas")
            .Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
                Background = value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null }));
        AddHandler(KeyDownEvent, OnShortcut, RoutingStrategies.Tunnel);
        _model.PropertyChanged += OnModelChanged;
        _model.LayoutChanged += OnLayoutChanged;
        _model.ErrorsChanged += OnErrorsChanged;
        _model.FocusRequested += OnFocusRequested;
        _model.Messages.CollectionChanged += OnMessagesChanged;
        _model.CommitEditors = CommitEditors;
        _model.TableGate = TableGate;
        Refresh();
    }

    /// <summary>The form this view shows.</summary>
    public DocumentFormViewModel Model => _model;

    /// <summary>The toolbar, for tests and the shell's focus handling.</summary>
    public StackPanel Toolbar => _toolbar;

    /// <summary>The tab strip of a multi-tab layout; null for a single-tab layout.</summary>
    public TabControl? Tabs => _tabs;

    /// <summary>The control that edits <paramref name="field"/>, or null when it is not shown.</summary>
    public Control? ControlOf(string field) => _bindings.FirstOrDefault(b => b.Field.Name == field)?.Control
        ?? _tables.FirstOrDefault(t => t.Model.Table.Field.Name == field);

    /// <summary>The label wrapper of <paramref name="field"/> (carries the error text).</summary>
    public SpotField? WrapperOf(string field) => _bindings.FirstOrDefault(b => b.Field.Name == field)?.Wrapper;

    /// <summary>The child-table control of a table field, or null.</summary>
    public SpotChildTable? TableOf(string field) => _tables.FirstOrDefault(t => t.Model.Table.Field.Name == field);

    /// <summary>How many columns the first section uses at the current width.</summary>
    public int ColumnCount => Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(this).OfType<ColumnsPanel>().FirstOrDefault()?.ColumnCount ?? 0;

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
        _model.PropertyChanged -= OnModelChanged;
        _model.LayoutChanged -= OnLayoutChanged;
        _model.ErrorsChanged -= OnErrorsChanged;
        _model.FocusRequested -= OnFocusRequested;
        _model.Messages.CollectionChanged -= OnMessagesChanged;
        _model.CommitEditors = null;
        _model.TableGate = null;
        DisposeTables();
    }

    // ------------------------------------------------------------------ chrome

    private void Refresh()
    {
        _title.Text = _model.Title;
        _badge.Text = _model.StatusBadge;
        AutomationProperties.SetName(this, _model.Title);
        RefreshToolbar();
        RefreshConflict();
        RefreshMessages();
        RefreshAudit();
        _recovery.Content = _model.Recovery;
        _note.Text = _model.HistoryNote;
        _note.IsVisible = !string.IsNullOrEmpty(_model.HistoryNote);
        if (_model.Phase == FormPhase.Failed)
        {
            _body.Content = new TextBlock { Text = _model.LoadError ?? "The document could not be opened.", TextWrapping = TextWrapping.Wrap };
        }
        else if (_model.Phase == FormPhase.Loading)
        {
            _body.Content = new TextBlock { Text = "Loading…" };
        }
        else if (_model.Layout is not null && _bindings.Count == 0 && _tables.Count == 0)
        {
            BuildBody();
        }
    }

    private void RefreshToolbar()
    {
        _toolbar.Children.Clear();
        foreach (var action in _model.Actions)
        {
            var button = new SpotButton
            {
                Content = action.Label,
                IsEnabled = action.IsEnabled,
                Tag = action.Id,
            };
            AutomationProperties.SetName(button, action.Label);
            ToolTip.SetTip(button, action.IsEnabled ? action.Gesture : action.DisabledReason);
            var id = action.Id;
            button.Click += async (_, _) => await _model.RunActionAsync(id);
            _toolbar.Children.Add(button);
        }
    }

    private void RefreshConflict()
    {
        var info = _model.Conflict;
        _conflict.IsVisible = info is not null;
        if (info is null)
        {
            _conflict.Child = null;
            return;
        }

        var text = new TextBlock
        {
            TextWrapping = TextWrapping.Wrap,
            Text = $"This document changed on the server (your version {info.YourVersion}"
                + (info.CurrentVersion is { } current ? $", server version {current}" : string.Empty)
                + "). Nothing was overwritten. Reload the latest and re-apply your changes, or overwrite after checking.",
        };
        var reload = new SpotButton { Content = "Reload and re-apply my changes" };
        reload.Click += async (_, _) => await _model.ResolveConflictAsync(ConflictChoice.ReloadAndReapply);
        var overwrite = new SpotButton { Content = "Overwrite with my changes" };
        overwrite.Click += async (_, _) => await _model.ResolveConflictAsync(ConflictChoice.Overwrite);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = Metric("Space.2", 8) };
        buttons.Children.Add(reload);
        buttons.Children.Add(overwrite);
        var panel = new StackPanel { Spacing = Metric("Space.2", 8) };
        panel.Children.Add(text);
        panel.Children.Add(buttons);
        AutomationProperties.SetName(panel, "Version conflict");
        _conflict.Child = panel;
    }

    private void RefreshMessages()
    {
        _messages.ItemsSource = _model.Messages
            .Select(m => $"{(m.Level == "error" ? "Error" : m.Level == "warning" ? "Warning" : "Info")}: {m.Text}")
            .ToList();
        AutomationProperties.SetName(_messages, "Messages");
    }

    private void RefreshAudit()
    {
        var audit = _model.Audit;
        _audit.IsVisible = !audit.IsEmpty;
        _audit.Text = $"Created {Join(audit.CreatedAt, audit.CreatedBy)}  ·  Modified {Join(audit.ModifiedAt, audit.ModifiedBy)}";
    }

    private static string Join(string? at, string? by)
        => (at, by) switch
        {
            (null, null) => "-",
            (_, null) => at!,
            (null, _) => "by " + by,
            _ => $"{at} by {by}",
        };

    // ------------------------------------------------------------------ body

    private void BuildBody()
    {
        DisposeTables();
        _bindings.Clear();
        _tabs = null;
        var layout = _model.Layout;
        var draft = _model.Draft;
        if (layout is null || draft is null)
        {
            return;
        }

        if (layout.Tabs.Count <= 1)
        {
            _body.Content = new ScrollViewer
            {
                Content = Sections(layout.Tabs.Count == 1 ? layout.Tabs[0].Sections : [], draft),
                HorizontalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Disabled,
            };
            return;
        }

        _tabs = new TabControl();
        var items = new List<TabItem>();
        foreach (var tab in layout.Tabs)
        {
            items.Add(new TabItem
            {
                Header = tab.Label,
                Content = new ScrollViewer
                {
                    Content = Sections(tab.Sections, draft),
                    HorizontalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Disabled,
                },
            });
        }

        _tabs.ItemsSource = items;
        _body.Content = _tabs;
    }

    private StackPanel Sections(IReadOnlyList<FormSection> sections, DocumentDraft draft)
    {
        var stack = new StackPanel { Spacing = Metric("Space.5", 20) };
        foreach (var section in sections)
        {
            var block = new StackPanel { Spacing = Metric("Space.3", 12) };
            if (!string.IsNullOrEmpty(section.Label))
            {
                var header = new TextBlock { Text = section.Label, FontWeight = FontWeight.SemiBold };
                AutomationProperties.SetName(header, section.Label);
                block.Children.Add(header);
            }

            var flow = new ColumnsPanel();
            foreach (var field in section.Fields)
            {
                if (field.Kind == FieldKind.Table && string.IsNullOrEmpty(field.LinkField))
                {
                    if (flow.Children.Count > 0)
                    {
                        block.Children.Add(flow);
                        flow = new ColumnsPanel();
                    }

                    block.Children.Add(TableBlock(field, draft));
                    continue;
                }

                flow.Children.Add(FieldBlock(field, draft));
            }

            if (flow.Children.Count > 0)
            {
                block.Children.Add(flow);
            }

            stack.Children.Add(block);
        }

        return stack;
    }

    private SpotField FieldBlock(FieldModel field, DocumentDraft draft)
    {
        var state = draft.Fields[field.Name];
        var control = FieldControlFactory.Create(field, state, draft, field.Name);
        if (control is SpotLookupBox lookup)
        {
            lookup.Source = _model.LookupFor(field);
            lookup.PointerPressed += (_, e) =>
            {
                if (e.KeyModifiers.HasFlag(KeyModifiers.Control))
                {
                    _model.OpenLink(field.Name);
                    e.Handled = true;
                }
            };
        }

        if (_model.IsReadOnly)
        {
            ReadOnlyPresentation(control);
        }

        var wrapper = new SpotField
        {
            Label = field.Label,
            IsRequired = field.Required || state.IsMandatory,
            HelpText = field.Hints.Description,
            ErrorText = ErrorTextOf(state),
            Content = control,
        };
        _bindings.Add(new FieldBinding(field, control, state, wrapper));
        if (field.Kind is FieldKind.Link && field.LinkTo is not null && control is not SpotMaskedField)
        {
            var open = new SpotButton { Content = "Open", VerticalAlignment = VerticalAlignment.Center };
            AutomationProperties.SetName(open, "Open " + field.Label);
            open.Click += (_, _) => _model.OpenLink(field.Name);
            var row = new DockPanel { LastChildFill = true };
            DockPanel.SetDock(open, Dock.Right);
            row.Children.Add(open);
            row.Children.Add(control);
            wrapper.Content = row;
        }

        return wrapper;
    }

    private StackPanel TableBlock(FieldModel field, DocumentDraft draft)
    {
        var table = new SpotChildTable(draft, field.Name, null, _model.LookupFor)
        {
            CanWrite = !_model.IsReadOnly,
            History = _model.History?.ForTable(field.Name),
        };
        _tables.Add(table);
        FitTable(table, draft);
        void Fit(object? sender, StructureChangedEventArgs e) => FitTable(table, draft);
        draft.StructureChanged += Fit;
        _tableHandlers.Add((draft, Fit));
        var label = new TextBlock { Text = field.Label, FontWeight = FontWeight.SemiBold };
        AutomationProperties.SetName(label, field.Label);
        return new StackPanel { Spacing = Metric("Space.2", 8), Children = { label, table } };
    }

    /// <summary>Sizes the table to its rows (up to <see cref="MaxTableRows"/>, then it scrolls). The
    /// grid shows placeholders for rows it has not fetched, and it fetches only as many rows as the
    /// viewport holds, so a viewport taller than the table would never show its rows.</summary>
    private static void FitTable(SpotChildTable table, DocumentDraft draft)
    {
        var rows = Math.Clamp(draft.Table(table.Model.Table.Field.Name).Rows.Count, 1, MaxTableRows);
        var compact = Density.GetMode(table) == DensityMode.Compact;
        var rowHeight = Metric(compact ? "Density.Compact.Grid.RowHeight" : "Density.Comfortable.Grid.RowHeight", compact ? 28 : 36);
        table.Height = rowHeight * (rows + 1);
    }

    private const int MaxTableRows = 10;

    private static string? ErrorTextOf(FieldState state)
        => state.Errors.Count == 0 ? null : string.Join("; ", state.Errors.Select(e => e.Message));

    private static void ReadOnlyPresentation(Control control)
    {
        switch (control)
        {
            case SpotMaskedField:
                break;
            case SpotTextInput input:
                input.IsReadOnly = true;
                break;
            case CatalogueControl catalogue:
                catalogue.IsReadOnly = true;
                break;
            case TextBox text:
                text.IsReadOnly = true;
                break;
            default:
                control.IsEnabled = false;
                break;
        }
    }

    private void DisposeTables()
    {
        foreach (var (draft, handler) in _tableHandlers)
        {
            draft.StructureChanged -= handler;
        }

        _tableHandlers.Clear();
        foreach (var table in _tables)
        {
            table.Dispose();
        }

        _tables.Clear();
    }

    /// <summary>The table's own editable check, handed to the form's history: an undo or a redo of a
    /// row step takes exactly the gate an edit does (write right, document state, masks).</summary>
    private ChildTableResult? TableGate(string table)
        => TableOf(table) is { } control
            ? control.Model.CheckEditable()
            : ChildTableResult.Refused(ChildTableRefusal.OutOfRange, $"The table {table} is not shown, so the change is refused.");

    private bool CommitEditors()
    {
        var valid = true;
        foreach (var binding in _bindings)
        {
            if (binding.Control is SpotMaskedField)
            {
                continue;
            }

            valid &= FieldControlFactory.GetBinding(binding.Control).CommitTo(binding.State);
        }

        OnErrorsChanged(this, EventArgs.Empty);
        return valid;
    }

    // ------------------------------------------------------------------ events

    private void OnModelChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(DocumentFormViewModel.Phase) or nameof(DocumentFormViewModel.Layout))
        {
            _bindings.Clear();
            DisposeTables();
        }

        Refresh();
    }

    private void OnLayoutChanged(object? sender, EventArgs e)
    {
        BuildBody();
        Refresh();
    }

    private void OnErrorsChanged(object? sender, EventArgs e)
    {
        foreach (var binding in _bindings)
        {
            binding.Wrapper.ErrorText = ErrorTextOf(binding.State);
        }

        RefreshMessages();
    }

    private void OnMessagesChanged(object? sender, NotifyCollectionChangedEventArgs e) => RefreshMessages();

    private void OnFocusRequested(object? sender, string field)
    {
        var target = ControlOf(field);
        if (target is null)
        {
            return;
        }

        if (target is SpotChildTable table)
        {
            table.Grid.Focus();
            return;
        }

        target.Focus();
    }

    private async void OnShortcut(object? sender, KeyEventArgs e)
    {
        var control = e.KeyModifiers.HasFlag(KeyModifiers.Control);
        if (control && e.Key == Key.S)
        {
            e.Handled = true;
            await _model.RunActionAsync("save");
        }
        else if (control && e.Key is Key.Enter)
        {
            e.Handled = true;
            await _model.RunActionAsync("submit");
        }
        else if (control && e.Key == Key.N)
        {
            e.Handled = true;
            await _model.RunActionAsync("new");
        }
        else if (e.Key == Key.F6)
        {
            e.Handled = true;
            CycleRegion(e.KeyModifiers.HasFlag(KeyModifiers.Shift) ? -1 : 1);
        }
        else if (e.KeyModifiers.HasFlag(KeyModifiers.Alt) && e.Key is >= Key.D1 and <= Key.D9 && _tabs is not null)
        {
            var index = e.Key - Key.D1;
            if (index < _tabs.ItemCount)
            {
                e.Handled = true;
                _tabs.SelectedIndex = index;
            }
        }
        else if (e.Key == Key.Escape && _model.Conflict is not null)
        {
            e.Handled = true;
            _conflict.IsVisible = false;
        }
    }

    /// <summary>Moves the focus to the next region: toolbar, form body, messages.</summary>
    public void CycleRegion(int step)
    {
        _region = (_region + step + 3) % 3;
        switch (_region)
        {
            case 0:
                _toolbar.Children.OfType<Control>().FirstOrDefault(c => c.IsEnabled)?.Focus();
                break;
            case 1:
                (_bindings.FirstOrDefault()?.Control ?? _tables.FirstOrDefault()?.Grid)?.Focus();
                break;
            default:
                _messages.Focus();
                break;
        }
    }

    private static double Metric(string token, double fallback)
        => Application.Current is { } app && app.TryGetResource(token, Avalonia.Styling.ThemeVariant.Light, out var value) && value is double d
            ? d
            : fallback;
}
