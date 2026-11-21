using Avalonia;
using Avalonia.Markup.Xaml;
using Avalonia.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>
/// Resolves a numeric design-token resource (a <see cref="double"/> key from
/// Spotledger.Client.Theme, e.g. <c>Space.1</c>) into a <see cref="Thickness"/>,
/// the way <c>DynamicResource</c> resolves it into a double property. The spacing,
/// density and radius tokens are theme-variant-invariant (they live outside the
/// Light/Dark dictionaries), so resolving at load time is equivalent to a dynamic
/// reference — and colours still use DynamicResource directly.
/// </summary>
public sealed class ThicknessTokenExtension : MarkupExtension
{
    public ThicknessTokenExtension()
    {
    }

    public ThicknessTokenExtension(string key) => Key = key;

    [ConstructorArgument("key")]
    public string? Key { get; set; }

    public override object ProvideValue(IServiceProvider serviceProvider)
        => new Thickness(TokenLookup.GetDouble(Key));
}

/// <summary>Same as <see cref="ThicknessTokenExtension"/> for corner-radius tokens.</summary>
public sealed class CornerRadiusTokenExtension : MarkupExtension
{
    public CornerRadiusTokenExtension()
    {
    }

    public CornerRadiusTokenExtension(string key) => Key = key;

    [ConstructorArgument("key")]
    public string? Key { get; set; }

    public override object ProvideValue(IServiceProvider serviceProvider)
        => new CornerRadius(TokenLookup.GetDouble(Key));
}

internal static class TokenLookup
{
    internal static double GetDouble(string? key)
    {
        if (string.IsNullOrEmpty(key))
        {
            throw new InvalidOperationException("A token key is required (e.g. Space.1).");
        }

        var app = global::Avalonia.Application.Current
            ?? throw new InvalidOperationException("Design tokens require a running Avalonia application.");

        // Spacing, radius and density tokens live at the dictionary root (they are
        // theme-variant-invariant), so resolve with the Light fallback.
        if (!app.TryGetResource(key, global::Avalonia.Styling.ThemeVariant.Light, out var value))
        {
            throw new InvalidOperationException($"Design token '{key}' is not in the application resources.");
        }

        return value switch
        {
            double d => d,
            _ => throw new InvalidOperationException($"Design token '{key}' must be a double, was {value?.GetType().Name ?? "null"}."),
        };
    }
}
