using Avalonia;
using Avalonia.Media;
using Avalonia.Media.Immutable;
using Avalonia.Media.TextFormatting;
using Avalonia.Styling;

namespace Spotledger.Client.FlowDesigner;

/// The theme lookups the canvas paints with: a colour, a number or a typeface from the Spotledger
/// token resources, with a fallback so the control also draws in a bare headless test app. The
/// canvas owns no colours of its own beyond the two tints it needs for a problem badge and a
/// permitted connection, which come from the semantic tokens where they exist.
internal static class FlowTokens
{

    public static double Number(string key, double fallback)
        => Application.Current is { } app
            && app.TryGetResource(key, ActiveVariant, out object? value)
            && value is double number
                ? number
                : fallback;

    public static IBrush Brush(string key, IBrush? fallback = null)
        => Application.Current is { } app
            && app.TryGetResource(key, ActiveVariant, out object? value)
            && value is Color color
                ? new ImmutableSolidColorBrush(color)
                : fallback ?? Brushes.Transparent;

    public static string FamilyName
        => Application.Current is { } app
            && app.TryGetResource("Font.Family.Latin", ActiveVariant, out object? value)
            && value is string family
                ? family
                : FontFamily.Default.Name;

    public static Typeface Typeface(FontWeight weight) => new(FamilyName, weight: weight);

    public static FormattedText Text(string text, Typeface typeface, double size, IBrush brush) =>
        new(
            text,
            System.Globalization.CultureInfo.CurrentCulture,
            FlowDirection.LeftToRight,
            typeface,
            size,
            brush);

    public static Pen Line(IBrush brush, double thickness = 1d, IDashStyle? dash = null) =>
        new(brush, thickness, dash);

    public static ThemeVariant ActiveVariant
        => Application.Current is { } app
            && app.ActualThemeVariant is { } actual
            && !Equals(actual, ThemeVariant.Default)
                ? actual
                : ThemeVariant.Light;
}
