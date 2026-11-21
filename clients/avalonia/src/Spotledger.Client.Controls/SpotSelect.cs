using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Data;

namespace Spotledger.Client.Controls;

/// <summary>
/// A combo box over <see cref="SelectOption"/> items. Type-to-search is Avalonia's built-in
/// <see cref="P:Avalonia.Controls.Primitives.SelectingItemsControl.IsTextSearchEnabled"/>
/// (turned on here), which matches the typed prefix against <c>ToString()</c> — and
/// <see cref="SelectOption.ToString"/> returns the label.
/// </summary>
public class SpotSelect : ComboBox
{
    public static readonly StyledProperty<string?> ErrorTextProperty =
        AvaloniaProperty.Register<SpotSelect, string?>(nameof(ErrorText));

    static SpotSelect()
    {
        IsTextSearchEnabledProperty.OverrideDefaultValue<SpotSelect>(true);
    }

    /// <summary>The validation message; non-null shows the danger border.</summary>
    public string? ErrorText
    {
        get => GetValue(ErrorTextProperty);
        set => SetValue(ErrorTextProperty, value);
    }

    /// <summary>The choices, in display order.</summary>
    public IReadOnlyList<SelectOption>? Options
    {
        get => _options;
        set
        {
            _options = value;
            ItemsSource = value;
        }
    }

    private IReadOnlyList<SelectOption>? _options;

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ErrorTextProperty)
        {
            PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        }
    }

    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        base.OnApplyTemplate(e);
        PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
    }
}
