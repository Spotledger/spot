using Avalonia;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Styling;

namespace Spotledger.Client.Theme;

/// <summary>
/// Installs the Spotledger token dictionary and the bundled fonts, and switches between the
/// four token variants. Everything a host needs is here; a host (Desktop, Browser, a test)
/// calls <see cref="Apply"/> once and again whenever the user or the platform changes the
/// theme.
/// </summary>
public static class SpotledgerTheme
{
    /// <summary>
    /// Merges <c>Tokens.axaml</c> into the application resources and registers the bundled
    /// Urdu faces. Idempotent: calling it twice does not add the dictionary twice.
    /// </summary>
    public static void MergeTokenResources(Application application)
    {
        ArgumentNullException.ThrowIfNull(application);
        if (!IsMerged(application))
        {
            application.Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Theme/"))
            {
                Source = new Uri("avares://Spotledger.Client.Theme/Tokens.axaml")
            });
        }

        SpotledgerFonts.Register();
    }

    /// <summary>
    /// Merges the tokens (if needed) and sets the requested theme variant. The high-contrast
    /// variants are custom <see cref="ThemeVariant"/> keys that inherit from Light and Dark,
    /// so built-in control themes keep resolving.
    /// </summary>
    public static void Apply(Application application, ThemeVariantKind variant)
    {
        ArgumentNullException.ThrowIfNull(application);
        MergeTokenResources(application);
        application.RequestedThemeVariant = variant.ToThemeVariant();
    }

    /// <summary>
    /// Merges the tokens, asks the detector, and applies whatever the platform and the user's
    /// own choice add up to. This is the call a host makes at start-up and on a system
    /// settings change; wiring it into the Desktop app is a follow-up (the Desktop project is
    /// not owned here).
    /// </summary>
    public static ThemeVariantKind ApplyForContrast(
        Application application,
        IHighContrastDetector detector,
        ThemeVariantKind requested = ThemeVariantKind.Light)
    {
        ArgumentNullException.ThrowIfNull(application);
        ArgumentNullException.ThrowIfNull(detector);
        var variant = HighContrastDetector.Resolve(requested, detector.Read());
        Apply(application, variant);
        return variant;
    }

    /// <summary>The token variant the application is currently painting with.</summary>
    public static ThemeVariantKind ActiveVariant(Application application)
    {
        ArgumentNullException.ThrowIfNull(application);
        return ThemeVariants.FromThemeVariant(application.ActualThemeVariant);
    }

    /// <summary>Resolves a token resource in an explicit variant (never the ambient one).</summary>
    public static bool TryGetToken(Application application, string key, ThemeVariantKind variant, out object? value)
    {
        ArgumentNullException.ThrowIfNull(application);
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        return application.TryGetResource(key, variant.ToThemeVariant(), out value);
    }

    private static bool IsMerged(Application application) =>
        application.Resources.MergedDictionaries
            .OfType<ResourceInclude>()
            .Any(include => include.Source is { } source
                && source.AbsoluteUri == "avares://Spotledger.Client.Theme/Tokens.axaml");
}
