using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Metadata;
using Avalonia.Controls.Primitives;
using Avalonia.Data;

namespace Spotledger.Client.Controls;

/// <summary>
/// The form-field wrapper every metadata-driven form is built from: a label (with a required
/// marker), the input content, optional help text, and an error slot. Label goes above the
/// input at narrow widths and to the left of it at wide widths (the layout flips at a
/// configurable breakpoint). Gaps come from the <c>Density.*.Form.*</c> tokens, so switching
/// <see cref="Density.ModeProperty"/> changes the field's spacing.
/// </summary>
[TemplatePart("PART_Stacked", typeof(Panel))]
[TemplatePart("PART_Inline", typeof(Panel))]
public class SpotField : TemplatedControl
{
    public static readonly StyledProperty<string?> LabelProperty =
        AvaloniaProperty.Register<SpotField, string?>(nameof(Label));

    public static readonly StyledProperty<object?> ContentProperty =
        AvaloniaProperty.Register<SpotField, object?>(nameof(Content));

    public static readonly StyledProperty<string?> HelpTextProperty =
        AvaloniaProperty.Register<SpotField, string?>(nameof(HelpText));

    public static readonly StyledProperty<string?> ErrorTextProperty =
        AvaloniaProperty.Register<SpotField, string?>(nameof(ErrorText));

    public static readonly StyledProperty<bool> IsRequiredProperty =
        AvaloniaProperty.Register<SpotField, bool>(nameof(IsRequired));

    public static readonly StyledProperty<double> WideBreakpointProperty =
        AvaloniaProperty.Register<SpotField, double>(nameof(WideBreakpoint), 320);

    public static readonly DirectProperty<SpotField, bool> IsWideProperty =
        AvaloniaProperty.RegisterDirect<SpotField, bool>(nameof(IsWide), field => field.IsWide);

    private bool _isWide;

    /// <summary>The field caption.</summary>
    public string? Label
    {
        get => GetValue(LabelProperty);
        set => SetValue(LabelProperty, value);
    }

    /// <summary>The input control.</summary>
    [Avalonia.Metadata.Content]
    public object? Content
    {
        get => GetValue(ContentProperty);
        set => SetValue(ContentProperty, value);
    }

    /// <summary>Optional guidance shown below the input.</summary>
    public string? HelpText
    {
        get => GetValue(HelpTextProperty);
        set => SetValue(HelpTextProperty, value);
    }

    /// <summary>The validation message slot, shown in the danger colour when set.</summary>
    public string? ErrorText
    {
        get => GetValue(ErrorTextProperty);
        set => SetValue(ErrorTextProperty, value);
    }

    /// <summary>Shows the required marker <c>*</c> after the label.</summary>
    public bool IsRequired
    {
        get => GetValue(IsRequiredProperty);
        set => SetValue(IsRequiredProperty, value);
    }

    /// <summary>Width at (and above) which the label moves to the left of the input.</summary>
    public double WideBreakpoint
    {
        get => GetValue(WideBreakpointProperty);
        set => SetValue(WideBreakpointProperty, value);
    }

    /// <summary>True when the field currently lays label-out-beside (wide) instead of above.</summary>
    public bool IsWide
    {
        get => _isWide;
        private set => SetAndRaise(IsWideProperty, ref _isWide, value);
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ErrorTextProperty)
        {
            PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        }
        else if (change.Property == WideBreakpointProperty)
        {
            UpdateWideState(Bounds.Width);
        }
        else if (change.Property == BoundsProperty)
        {
            UpdateWideState(Bounds.Width);
        }
    }

    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        base.OnApplyTemplate(e);
        PseudoClasses.Set(":error", !string.IsNullOrEmpty(ErrorText));
        UpdateWideState(Bounds.Width);
    }

    private void UpdateWideState(double width)
    {
        var wide = width >= WideBreakpoint;
        IsWide = wide;
        PseudoClasses.Set(":wide", wide);
        PseudoClasses.Set(":narrow", !wide);
    }
}
