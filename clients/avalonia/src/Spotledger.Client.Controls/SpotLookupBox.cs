using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Data;
using Avalonia.Input;
using Avalonia.Interactivity;
using System.Windows.Input;
using Spotledger.Client.Metadata;
using Avalonia.Markup.Xaml.MarkupExtensions;

namespace Spotledger.Client.Controls;

/// <summary>Cancellable type-ahead. Search generations guard against transports ignoring cancellation.</summary>
public sealed class SpotLookupBox : CatalogueControl, IDisposable
{
    public static readonly StyledProperty<string?> SearchTextProperty = AvaloniaProperty.Register<SpotLookupBox, string?>(nameof(SearchText), defaultBindingMode: BindingMode.TwoWay);
    public static readonly StyledProperty<LookupItem?> SelectedItemProperty = AvaloniaProperty.Register<SpotLookupBox, LookupItem?>(nameof(SelectedItem), defaultBindingMode: BindingMode.TwoWay);
    public string? SearchText { get => GetValue(SearchTextProperty); set => SetValue(SearchTextProperty, value); }
    public LookupItem? SelectedItem { get => GetValue(SelectedItemProperty); set => SetValue(SelectedItemProperty, value); }
    public ILookupSource? Source { get; set; }
    public ILookupDelay Delay { get; set; } = new LookupDelay();
    public IReadOnlyList<LookupItem> Items { get; private set; } = [];
    public IReadOnlyList<LookupColumn> Columns { get; private set; } = [];
    private bool _canCreateTarget;
    public bool CanCreateTarget { get => _canCreateTarget; set { _canCreateTarget = value; UpdatePresentation(); } }
    public ICommand? CreateCommand { get; set; }
    public object? CreateCommandParameter { get; set; }
    public void RequestCreate()
    {
        if (CanCreateTarget && !IsReadOnly && CreateCommand?.CanExecute(CreateCommandParameter) == true)
            CreateCommand.Execute(CreateCommandParameter);
    }
    public bool IsLoading { get; private set; }
    public string? ErrorText { get; private set; }
    public bool IsStale { get; private set; }
    public bool IsOpen { get; private set; }
    public int HighlightedIndex { get; private set; } = -1;
    public Task PendingSearch { get; private set; } = Task.CompletedTask;
    public string StatusText => ErrorText ?? (IsStale ? "Saved selection no longer resolves" : IsLoading ? "Loading…" : Items.Count == 0 ? "No match" : string.Empty);
    public TextBox Editor { get; } = new();
    private readonly TextBox _status = Display("Lookup status");
    private readonly StackPanel _rows = new();
    private readonly Button _create;
    private CancellationTokenSource? _searchCancellation;
    private CancellationTokenSource? _resolveCancellation;
    private int _generation;
    private int _resolveGeneration;
    private bool _sync;
    private bool _navigated;
    public event EventHandler? SelectionAccepted;
    public SpotLookupBox() : base("Lookup")
    {
        AutomationProperties.SetName(Editor, "Search");
        Body.Children.Add(Editor);
        Body.Children.Add(_status);
        Body.Children.Add(_rows);
        _create = Action("Create new", RequestCreate);
        Body.Children.Add(_create);
        Editor.PropertyChanged += (_, e) => { if (e.Property == TextBox.TextProperty && !_sync) SetCurrentValue(SearchTextProperty, Editor.Text); };
        AddHandler(KeyDownEvent, OnEditorKeyDown, RoutingStrategies.Tunnel);
        UpdatePresentation();
    }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == SearchTextProperty && !_sync && !IsReadOnly)
        {
            Editor.Text = SearchText;
            _navigated = false;
            HighlightedIndex = -1;
            CancelSearch();
            PendingSearch = SearchAsync(_generation, _searchCancellation!.Token);
        }
        if (change.Property == SelectedItemProperty)
        {
            IsStale = false;
            _resolveGeneration++;
            _resolveCancellation?.Cancel();
            SetText(SelectedItem?.Label);
            UpdatePresentation();
        }
    }
    private void CancelSearch()
    {
        _generation++;
        _searchCancellation?.Cancel();
        _searchCancellation?.Dispose();
        _searchCancellation = new CancellationTokenSource();
    }
    private async Task SearchAsync(int generation, CancellationToken token)
    {
        var text = SearchText ?? string.Empty;
        IsLoading = false;
        ErrorText = null;
        Items = [];
        Columns = [];
        IsOpen = true;
        UpdatePresentation();
        try
        {
            await Delay.WaitAsync(TimeSpan.FromMilliseconds(200), token);
            token.ThrowIfCancellationRequested();
            if (Source is null) { ErrorText = "Lookup source is not configured"; return; }
            IsLoading = true;
            UpdatePresentation();
            var result = await Source.SearchResultAsync(text, token);
            if (generation != _generation || token.IsCancellationRequested) return;
            var items = result.Rows.Select(row => new LookupItem(row.Id, row.Label, row.Description, row.Cells)).ToArray();
            Items = items;
            Columns = result.Columns;
            HighlightedIndex = items.Length > 0 ? 0 : -1;
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        catch (Exception) { if (generation == _generation) ErrorText = "Lookup failed. Try again."; }
        finally
        {
            if (generation == _generation) { IsLoading = false; UpdatePresentation(); }
        }
    }
    /// <summary>Resolve a persisted id separately from the user's current search.</summary>
    public async Task SetSelectionAsync(LookupItem? saved)
    {
        SelectedItem = saved;
        var generation = ++_resolveGeneration;
        _resolveCancellation?.Cancel();
        _resolveCancellation?.Dispose();
        _resolveCancellation = new CancellationTokenSource();
        var token = _resolveCancellation.Token;
        if (saved is null || Source is null) return;
        try
        {
            var resolved = await Source.ResolveAsync(saved.Id, token);
            if (generation != _resolveGeneration || token.IsCancellationRequested) return;
            if (resolved is not null) SelectedItem = resolved;
            else IsStale = true;
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        catch (Exception) { if (generation == _resolveGeneration) ErrorText = "Selection could not be checked. Try again."; }
        UpdatePresentation();
    }
    private void OnEditorKeyDown(object? sender, KeyEventArgs e)
    {
        var handled = HandleKey(e.Key);
        // Tab still moves focus after possibly accepting a navigated row.
        if (handled && e.Key != Key.Tab) e.Handled = true;
    }
    public bool HandleKey(Key key)
    {
        if (IsReadOnly) return false;
        if (key is Key.Down or Key.Up)
        {
            if (!IsOpen) { IsOpen = true; if (Items.Count == 0) { CancelSearch(); PendingSearch = SearchAsync(_generation, _searchCancellation!.Token); } }
            else if (Items.Count > 0)
            {
                HighlightedIndex = Math.Clamp(HighlightedIndex + (key == Key.Down ? 1 : -1), 0, Items.Count - 1);
                _navigated = true;
            }
            UpdatePresentation();
            return true;
        }
        if (key == Key.Escape) { Close(true); return true; }
        if (key == Key.Enter && IsOpen) { Accept(); return true; }
        if (key == Key.Tab && IsOpen) { if (_navigated) Accept(); else Close(true); return true; }
        return false;
    }
    private void Accept()
    {
        if (HighlightedIndex >= 0 && HighlightedIndex < Items.Count)
        {
            SetCurrentValue(SelectedItemProperty, Items[HighlightedIndex]);
            SelectionAccepted?.Invoke(this, EventArgs.Empty);
        }
        Close(true);
    }
    public void Close(bool restore)
    {
        CancelSearch();
        IsLoading = false;
        IsOpen = false;
        _navigated = false;
        if (restore) SetText(SelectedItem?.Label);
        UpdatePresentation();
    }
    private void SetText(string? text)
    {
        _sync = true;
        try { SetCurrentValue(SearchTextProperty, text); Editor.Text = text; }
        finally { _sync = false; }
    }
    protected override void UpdatePresentation()
    {
        if (Editor is null || _create is null) return;
        _create.IsVisible = CanCreateTarget && !IsReadOnly;
        Editor.IsReadOnly = IsReadOnly;
        if (IsReadOnly) { IsOpen = false; IsLoading = false; _searchCancellation?.Cancel(); }
        _status.Text = StatusText;
        _status.IsVisible = IsStale || (IsOpen && StatusText.Length > 0);
        _rows.IsVisible = IsOpen && !IsReadOnly;
        _rows.Children.Clear();
        if (Columns.Count > 0) _rows.Children.Add(PickerColumns.Create(Columns, null));
        for (var i = 0; i < Items.Count; i++)
        {
            var index = i;
            var item = Items[i];
            var row = Action((i == HighlightedIndex ? "› " : string.Empty) + item.Label + (item.Description is null ? string.Empty : "\n" + item.Description), () => { HighlightedIndex = index; Accept(); });
            if (Columns.Count > 0)
            {
                row.Content = PickerColumns.Create(Columns, item);
                row.HorizontalContentAlignment = Avalonia.Layout.HorizontalAlignment.Stretch;
                if (i == HighlightedIndex) row.Bind(BackgroundProperty, new DynamicResourceExtension("Color.Brand.100"));
            }
            _rows.Children.Add(row);
        }
    }
    public void Dispose()
    {
        _generation++;
        _resolveGeneration++;
        _searchCancellation?.Cancel();
        _searchCancellation?.Dispose();
        _resolveCancellation?.Cancel();
        _resolveCancellation?.Dispose();
        _searchCancellation = null;
        _resolveCancellation = null;
    }
    protected override void OnDetachedFromVisualTree(VisualTreeAttachmentEventArgs e)
    {
        Close(true);
        _resolveGeneration++;
        _resolveCancellation?.Cancel();
        base.OnDetachedFromVisualTree(e);
    }
}
