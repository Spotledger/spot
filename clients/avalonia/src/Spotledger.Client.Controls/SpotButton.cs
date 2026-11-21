using System.Diagnostics.CodeAnalysis;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Interactivity;
using Avalonia.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>Visual role of a <see cref="SpotButton"/>.</summary>
public enum SpotButtonVariant
{
    Primary,
    Secondary,
    Danger,
    Subtle
}

/// <summary>
/// The design-system button: variants, a leading icon slot, a busy state that shows a
/// progress ring and swallows clicks, and a visible focus ring. All colours, sizes and
/// shapes come from Theme tokens through the ControlTheme.
/// </summary>
public class SpotButton : Button
{
    public static readonly StyledProperty<SpotButtonVariant> VariantProperty =
        AvaloniaProperty.Register<SpotButton, SpotButtonVariant>(nameof(Variant), SpotButtonVariant.Primary);

    public static readonly StyledProperty<object?> IconProperty =
        AvaloniaProperty.Register<SpotButton, object?>(nameof(Icon));

    public static readonly StyledProperty<bool> IsBusyProperty =
        AvaloniaProperty.Register<SpotButton, bool>(nameof(IsBusy));

    /// <summary>The visual variant; drives the ControlTheme selectors.</summary>
    public SpotButtonVariant Variant
    {
        get => GetValue(VariantProperty);
        set => SetValue(VariantProperty, value);
    }

    /// <summary>Optional leading icon content (mirrored automatically in RTL).</summary>
    [AllowNull]
    public object? Icon
    {
        get => GetValue(IconProperty);
        set => SetValue(IconProperty, value);
    }

    /// <summary>Shows the progress indicator, disables pointer input and blocks commands.</summary>
    public bool IsBusy
    {
        get => GetValue(IsBusyProperty);
        set => SetValue(IsBusyProperty, value);
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == IsBusyProperty)
        {
            PseudoClasses.Set(":busy", IsBusy);
        }
        else if (change.Property == VariantProperty)
        {
            PseudoClasses.Set(":primary", Variant == SpotButtonVariant.Primary);
            PseudoClasses.Set(":secondary", Variant == SpotButtonVariant.Secondary);
            PseudoClasses.Set(":danger", Variant == SpotButtonVariant.Danger);
            PseudoClasses.Set(":subtle", Variant == SpotButtonVariant.Subtle);
        }
    }

    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        base.OnApplyTemplate(e);
        // Re-assert pseudo-classes so a control constructed before theming still styles right.
        PseudoClasses.Set(":busy", IsBusy);
        PseudoClasses.Set(":primary", Variant == SpotButtonVariant.Primary);
        PseudoClasses.Set(":secondary", Variant == SpotButtonVariant.Secondary);
        PseudoClasses.Set(":danger", Variant == SpotButtonVariant.Danger);
        PseudoClasses.Set(":subtle", Variant == SpotButtonVariant.Subtle);
    }

    protected override void OnClick()
    {
        // A busy button never raises Click nor runs its command; the base pipeline stops here.
        if (IsBusy)
        {
            return;
        }

        base.OnClick();
    }
}
