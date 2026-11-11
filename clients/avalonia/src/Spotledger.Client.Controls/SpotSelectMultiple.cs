using Avalonia;
using Avalonia.Controls;
using Avalonia.Data;

namespace Spotledger.Client.Controls;

/// <summary>Multiple select texts, not links. The definition supplies the order and vocabulary.</summary>
public sealed class SpotSelectMultiple : CatalogueControl
{
    public static readonly StyledProperty<IReadOnlyList<string>> SelectedValuesProperty = AvaloniaProperty.Register<SpotSelectMultiple, IReadOnlyList<string>>(nameof(SelectedValues), Array.Empty<string>(), defaultBindingMode: BindingMode.TwoWay);
    public IReadOnlyList<string> SelectedValues { get => GetValue(SelectedValuesProperty); set => SetValue(SelectedValuesProperty, value); }
    private IReadOnlyList<string> _options = [];
    public IReadOnlyList<string> Options
    {
        get => _options;
        set { _options = value; UpdatePresentation(); }
    }
    private readonly WrapPanel _choices = new();
    private readonly TextBox _display = Display("Selected options");
    public SpotSelectMultiple() : base("Multiple options")
    {
        Body.Children.Add(_display);
        Body.Children.Add(_choices);
        UpdatePresentation();
    }
    public void Toggle(string text)
    {
        if (IsReadOnly || !Options.Contains(text, StringComparer.Ordinal)) return;
        var selected = SelectedValues.ToHashSet(StringComparer.Ordinal);
        if (!selected.Add(text)) selected.Remove(text);
        SetCurrentValue(SelectedValuesProperty, Options.Where(selected.Contains).ToArray());
    }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == SelectedValuesProperty) UpdatePresentation();
    }
    protected override void UpdatePresentation()
    {
        if (_choices is null) return;
        _display.Text = string.Join(", ", Options.Where(x => SelectedValues.Contains(x, StringComparer.Ordinal)));
        _display.IsVisible = IsReadOnly;
        _choices.IsVisible = !IsReadOnly;
        _choices.Children.Clear();
        foreach (var text in Options)
        {
            var chip = Action((SelectedValues.Contains(text, StringComparer.Ordinal) ? "✓ " : string.Empty) + text, () => Toggle(text));
            _choices.Children.Add(chip);
        }
    }
}
