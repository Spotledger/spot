using Avalonia.Styling;

namespace Spotledger.Client.Theme;

/// <summary>
/// The four token variants the theme ships. Light and Dark are the ordinary pair; the two
/// high-contrast variants carry the accessibility palette (every text pair at least 7:1)
/// and are only used when the platform reports high contrast on
/// (<see cref="HighContrastDetector"/>).
/// </summary>
public enum ThemeVariantKind
{
    Light,
    Dark,
    HighContrast,
    HighContrastDark,
}

/// <summary>
/// Maps <see cref="ThemeVariantKind"/> onto Avalonia's <see cref="ThemeVariant"/>.
///
/// <para>The high-contrast variants are custom keys in <c>Tokens.axaml</c>'s theme dictionaries
/// and inherit from Light and Dark respectively, so any control theme that only knows the
/// built-in pair still resolves while our tokens follow the high-contrast dictionary. Avalonia's
/// XAML type converter refuses a custom key written as text, so the generated dictionary uses
/// <c>x:Static</c> against the fields below — which is why they are fields with exactly these
/// names, one per non-built-in variant in <c>design/tokens.json</c>.</para>
/// </summary>
public static class ThemeVariants
{
    /// <summary>Theme dictionary key of the light high-contrast palette.</summary>
    public const string HighContrastKey = "HighContrast";

    /// <summary>Theme dictionary key of the dark high-contrast palette.</summary>
    public const string HighContrastDarkKey = "HighContrastDark";

    /// <summary>The light high-contrast variant: dark text on a light, fully opaque surface.</summary>
    public static readonly ThemeVariant HighContrast = new(HighContrastKey, ThemeVariant.Light);

    /// <summary>The dark high-contrast variant: light text on black, for a dark HC desktop.</summary>
    public static readonly ThemeVariant HighContrastDark = new(HighContrastDarkKey, ThemeVariant.Dark);

    /// <summary>The Avalonia variant for a token variant.</summary>
    public static ThemeVariant ToThemeVariant(this ThemeVariantKind kind) => kind switch
    {
        ThemeVariantKind.Light => ThemeVariant.Light,
        ThemeVariantKind.Dark => ThemeVariant.Dark,
        ThemeVariantKind.HighContrast => HighContrast,
        ThemeVariantKind.HighContrastDark => HighContrastDark,
        _ => throw new ArgumentOutOfRangeException(nameof(kind), kind, "Unknown theme variant."),
    };

    /// <summary>The token variant for an Avalonia variant, including a custom key.</summary>
    public static ThemeVariantKind FromThemeVariant(ThemeVariant? variant)
    {
        if (variant is null)
        {
            return ThemeVariantKind.Light;
        }

        if (variant == HighContrast || Equals(variant.Key, HighContrastKey))
        {
            return ThemeVariantKind.HighContrast;
        }

        if (variant == HighContrastDark || Equals(variant.Key, HighContrastDarkKey))
        {
            return ThemeVariantKind.HighContrastDark;
        }

        return variant == ThemeVariant.Dark ? ThemeVariantKind.Dark : ThemeVariantKind.Light;
    }

    /// <summary>True for either high-contrast palette.</summary>
    public static bool IsHighContrast(this ThemeVariantKind kind) =>
        kind is ThemeVariantKind.HighContrast or ThemeVariantKind.HighContrastDark;

    /// <summary>True when the palette paints light text on a dark background.</summary>
    public static bool IsDark(this ThemeVariantKind kind) =>
        kind is ThemeVariantKind.Dark or ThemeVariantKind.HighContrastDark;
}
