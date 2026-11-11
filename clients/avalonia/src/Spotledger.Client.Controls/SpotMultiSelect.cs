using Avalonia;
using Avalonia.Controls;
using Avalonia.Data;
using Avalonia.Input;
using Avalonia.Interactivity;
using System.Windows.Input;

namespace Spotledger.Client.Controls;

/// <summary>A link-chip editor for table_multiselect, not a multi-valued select.</summary>
public sealed class SpotMultiSelect : CatalogueControl
{
    public static readonly StyledProperty<IReadOnlyList<LookupItem>> SelectedItemsProperty = AvaloniaProperty.Register<SpotMultiSelect, IReadOnlyList<LookupItem>>(nameof(SelectedItems), Array.Empty<LookupItem>(), defaultBindingMode: BindingMode.TwoWay);
    public IReadOnlyList<LookupItem> SelectedItems { get => GetValue(SelectedItemsProperty); set => SetValue(SelectedItemsProperty, value); }
    public SpotLookupBox Lookup { get; } = new();
    public ILookupSource? Source { get => Lookup.Source; set => Lookup.Source = value; }
    public bool CanCreateTarget { get => Lookup.CanCreateTarget; set => Lookup.CanCreateTarget = value; }
    public ICommand? CreateCommand { get => Lookup.CreateCommand; set => Lookup.CreateCommand = value; }
    public object? CreateCommandParameter { get => Lookup.CreateCommandParameter; set => Lookup.CreateCommandParameter = value; }
    public void RequestCreate() => Lookup.RequestCreate();
    private readonly WrapPanel _chips = new();
    private readonly TextBox _display = Display("Selected links");
    public SpotMultiSelect() : base("Multiple links")
    {
        Body.Children.Add(_display);
        Body.Children.Add(_chips);
        Body.Children.Add(Lookup);
        Lookup.SelectionAccepted += (_, _) => { if (Lookup.SelectedItem is { } selected) Add(selected); Lookup.SelectedItem = null; };
        Lookup.AddHandler(KeyDownEvent, (_, e) => { if (HandleKey(e.Key)) e.Handled = true; }, RoutingStrategies.Tunnel);
        UpdatePresentation();
    }
    public void Add(LookupItem item)
    {
        ArgumentNullException.ThrowIfNull(item);
        if (!IsReadOnly && !SelectedItems.Any(x => x.Id == item.Id)) SetCurrentValue(SelectedItemsProperty, SelectedItems.Append(item).ToArray());
    }
    public void Remove(string id)
    {
        if (!IsReadOnly) SetCurrentValue(SelectedItemsProperty, SelectedItems.Where(x => x.Id != id).ToArray());
    }
    public bool HandleKey(Key key)
    {
        if (IsReadOnly || key != Key.Back || !string.IsNullOrEmpty(Lookup.SearchText) || SelectedItems.Count == 0) return false;
        Remove(SelectedItems[^1].Id);
        return true;
    }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == SelectedItemsProperty) UpdatePresentation();
    }
    protected override void UpdatePresentation()
    {
        if (Lookup is null) return;
        Lookup.IsReadOnly = IsReadOnly;
        Lookup.IsVisible = !IsReadOnly;
        _display.Text = string.Join(", ", SelectedItems.Select(x => x.Label));
        _display.IsVisible = IsReadOnly;
        _chips.IsVisible = !IsReadOnly;
        _chips.Children.Clear();
        foreach (var item in SelectedItems) _chips.Children.Add(Action(item.Label + " ×", () => Remove(item.Id)));
    }
}
