using Avalonia;
using Avalonia.Media;
using Avalonia.Media.Immutable;
using Avalonia.Media.TextFormatting;
using Avalonia.Styling;

namespace Spotledger.Client.Graph;

internal static class GraphTokens
{
    /// <summary>A numeric token (density row height, font size, radius) or the fallback.</summary>
    public static double Number(string key, double fallback)
        => Application.Current is { } app
            && app.TryGetResource(key, ActiveVariant, out var value)
            && value is double number
                ? number
                : fallback;

    /// <summary>A colour token as an immutable brush, or the fallback brush.</summary>
    public static IBrush Brush(string key, IBrush? fallback = null)
        => Application.Current is { } app
            && app.TryGetResource(key, ActiveVariant, out var value)
            && value is Color color
                ? new ImmutableSolidColorBrush(color)
                : fallback ?? Brushes.Transparent;

    /// <summary>The Latin font family token.</summary>
    public static string FamilyName
        => Application.Current is { } app
            && app.TryGetResource("Font.Family.Latin", ActiveVariant, out var value)
            && value is string family
                ? family
                : FontFamily.Default.Name;

    /// <summary>A typeface in the token family at the given weight.</summary>
    public static Typeface Typeface(FontWeight weight) => new(FamilyName, weight: weight);

    /// <summary>The one shared formatted-text construction (no drawing).</summary>
    public static FormattedText Text(string text, Typeface typeface, double size, IBrush brush, bool rtl)
        => new(
            text,
            System.Globalization.CultureInfo.CurrentCulture,
            rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight,
            typeface,
            size,
            brush);

    /// <summary>A hairline pen.</summary>
    public static Pen Line(IBrush brush, double thickness = 1d) => new(brush, thickness);

    /// <summary>The active theme variant (Light when the app has not chosen one).</summary>
    public static ThemeVariant ActiveVariant
        => Application.Current is { } app
            && app.ActualThemeVariant is { } actual
            && !Equals(actual, ThemeVariant.Default)
                ? actual
                : ThemeVariant.Light;
}
