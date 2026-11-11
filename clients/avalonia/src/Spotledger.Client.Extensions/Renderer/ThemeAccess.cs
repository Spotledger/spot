using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Extensions.Renderer;

/// <summary>
/// Reads the design system's tokens. Every colour, size, gap and font of the renderer comes
/// through here, by token key, so a theme or density change reflows the extension surfaces
/// without a literal in this project.
/// </summary>
internal static class ThemeAccess
{
    internal static double Number(Visual element, string key, double fallback) =>
        TryResource(element, key, out object? value) && value is double number ? number : fallback;

    internal static Thickness Padding(Visual element, string key, Thickness fallback)
    {
        double value = Number(element, key, double.NaN);
        return double.IsNaN(value) ? fallback : new Thickness(value);
    }

    /// <summary>The brush of a colour token, or null when the theme has no such token (the
    /// caller then leaves the property at its themed default).</summary>
    internal static IBrush? Brush(Visual element, string colorKey) =>
        TryResource(element, colorKey, out object? value) && value is Color color
            ? new SolidColorBrush(color)
            : null;

    internal static FontFamily Family(Visual element, string key) =>
        TryResource(element, key, out object? value) && value is FontFamily family
            ? family
            : FontFamily.Default;

    internal static FontWeight Weight(Visual element, string key, FontWeight fallback) =>
        TryResource(element, key, out object? value) && value is FontWeight weight ? weight : fallback;

    /// <summary>
    /// Looks a token up under the control's actual theme variant. A plain
    /// <c>TryFindResource(key)</c> reads the <c>Default</c> variant only, and the design
    /// system keeps its colours in the <c>Light</c>/<c>Dark</c> theme dictionaries, so the
    /// variant must be passed explicitly: without it a dark window would draw light tokens.
    /// </summary>
    private static bool TryResource(Visual element, string key, out object? value)
    {
        Avalonia.Styling.ThemeVariant variant = element.ActualThemeVariant;
        if (element.TryFindResource(key, variant, out value))
        {
            return true;
        }
        if (variant == Avalonia.Styling.ThemeVariant.Default)
        {
            foreach (Avalonia.Styling.ThemeVariant fallback in new[] { Avalonia.Styling.ThemeVariant.Light, Avalonia.Styling.ThemeVariant.Dark })
            {
                if (element.TryFindResource(key, fallback, out value))
                {
                    return true;
                }
            }
        }
        return element.TryFindResource(key, out value);
    }

    /// <summary>The gap a form-like stack uses at the current density.</summary>
    internal static double FieldGap(Visual element) =>
        Number(element, Density.GetMode(element) == DensityMode.Compact
            ? Tokens.TokenDensityCompactFormFieldGap
            : Tokens.TokenDensityComfortableFormFieldGap, 8);

    /// <summary>The cell padding a table-like surface uses at the current density.</summary>
    internal static double CellPadding(Visual element) =>
        Number(element, Density.GetMode(element) == DensityMode.Compact
            ? Tokens.TokenDensityCompactGridCellPadding
            : Tokens.TokenDensityComfortableGridCellPadding, 4);

    /// <summary>Applies the body text role to a text block.</summary>
    internal static void Body(TextBlock text, Visual owner)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(owner);
        text.FontSize = Number(owner, Tokens.TokenFontSizeBody, 14);
        text.FontFamily = Family(owner, Tokens.TokenFontFamilyLatin);
        ApplyForeground(text, owner, Tokens.TokenColorTextPrimary);
        text.TextWrapping = TextWrapping.Wrap;
    }

    /// <summary>Applies the caption role (secondary text) to a text block.</summary>
    internal static void Caption(TextBlock text, Visual owner)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(owner);
        text.FontSize = Number(owner, Tokens.TokenFontSizeCaption, 12);
        text.FontFamily = Family(owner, Tokens.TokenFontFamilyLatin);
        ApplyForeground(text, owner, Tokens.TokenColorTextSecondary);
        text.TextWrapping = TextWrapping.Wrap;
    }

    /// <summary>Applies the heading role to a text block.</summary>
    internal static void Heading(TextBlock text, Visual owner)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(owner);
        text.FontSize = Number(owner, Tokens.TokenFontSizeHeading, 20);
        text.FontWeight = Weight(owner, Tokens.TokenFontWeightSemibold, FontWeight.SemiBold);
        text.FontFamily = Family(owner, Tokens.TokenFontFamilyLatin);
        ApplyForeground(text, owner, Tokens.TokenColorTextPrimary);
        text.TextWrapping = TextWrapping.Wrap;
    }

    /// <summary>Applies the display role (a stat's number) to a text block.</summary>
    internal static void Display(TextBlock text, Visual owner)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(owner);
        text.FontSize = Number(owner, Tokens.TokenFontSizeDisplay, 28);
        text.FontWeight = Weight(owner, Tokens.TokenFontWeightBold, FontWeight.Bold);
        text.FontFamily = Family(owner, Tokens.TokenFontFamilyLatin);
        ApplyForeground(text, owner, Tokens.TokenColorTextPrimary);
        text.TextWrapping = TextWrapping.Wrap;
    }

    /// <summary>True when the text has Arabic-script characters, so it needs the Urdu family.</summary>
    internal static bool IsRightToLeftText(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        foreach (char character in text)
        {
            if (character is >= '\u0600' and <= '\u06FF' or >= '\u0750' and <= '\u077F' or >= '\uFB50' and <= '\uFDFF' or >= '\uFE70' and <= '\uFEFF')
            {
                return true;
            }
        }
        return false;
    }

    /// <summary>Switches a text block to the Urdu family when its text is Arabic script, so a
    /// label in either language fits at the narrowest width instead of falling back to tofu.</summary>
    internal static void ApplyScriptFont(TextBlock text, Visual owner, string content)
    {
        ArgumentNullException.ThrowIfNull(text);
        ArgumentNullException.ThrowIfNull(owner);
        if (IsRightToLeftText(content))
        {
            text.FontFamily = Family(owner, Tokens.TokenFontFamilyUrdu);
        }
    }

    /// <summary>Sets a text role's foreground from a colour token when the theme has one.</summary>
    private static void ApplyForeground(TextBlock text, Visual owner, string key)
    {
        if (Brush(owner, key) is { } brush)
        {
            text.Foreground = brush;
        }
    }
}
