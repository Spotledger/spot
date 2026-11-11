using Avalonia;
using Avalonia.Controls;

namespace Spotledger.Client.Controls;

/// <summary>
/// Attached, inheritable <see cref="DensityMode"/> property. Set it on a container (a
/// <see cref="ControlGallery"/>, a form, a whole window) and every Spot control underneath
/// picks up the matching <c>Density.*</c> token set: the changed handler below mirrors the
/// mode into the <c>compact</c> style class that the ControlThemes switch on. The default
/// is Comfortable.
/// </summary>
public static class Density
{
    /// <summary>The style class added to every element whose effective mode is Compact.</summary>
    public const string CompactClass = "compact";

    public static readonly AttachedProperty<DensityMode> ModeProperty =
        AvaloniaProperty.RegisterAttached<Control, Visual, DensityMode>(
            "Mode",
            defaultValue: DensityMode.Comfortable,
            inherits: true);

    static Density()
    {
        ModeProperty.Changed.AddClassHandler<StyledElement>((element, change) =>
        {
            var compact = change.NewValue is DensityMode mode && mode == DensityMode.Compact;
            if (compact)
            {
                element.Classes.Add(CompactClass);
            }
            else
            {
                element.Classes.Remove(CompactClass);
            }
        });
    }

    public static DensityMode GetMode(Visual element)
    {
        ArgumentNullException.ThrowIfNull(element);
        return element.GetValue(ModeProperty);
    }

    public static void SetMode(Visual element, DensityMode value)
    {
        ArgumentNullException.ThrowIfNull(element);
        element.SetValue(ModeProperty, value);
    }
}
