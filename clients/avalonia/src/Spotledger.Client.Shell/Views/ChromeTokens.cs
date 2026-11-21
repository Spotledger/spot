using System;
using Avalonia;
using Avalonia.Markup.Xaml;
using Avalonia.Metadata;

namespace Spotledger.Client.Shell.Views;

/// <summary>
/// Resolves a numeric Theme token (a <c>double</c> key such as <c>Space.4</c>) to a double
/// for properties that take a plain value (Width, Height, Spacing). Spacing tokens are
/// theme-variant-invariant (they live outside the Light/Dark dictionaries), so resolving
/// at load time equals a dynamic reference — the same approach the Controls library uses
/// for its Thickness/CornerRadius token extensions. Colours stay with DynamicResource.
/// </summary>
public sealed class DoubleTokenExtension : MarkupExtension
{
    public DoubleTokenExtension()
    {
    }

    public DoubleTokenExtension(string key) => Key = key;

    [ConstructorArgument("key")]
    public string? Key { get; set; }

    public override object ProvideValue(IServiceProvider serviceProvider) => TokenResolver.GetDouble(Key);
}

/// <summary>
/// A thickness composed from numeric token keys so the chrome never writes a literal
/// metric: <c>{views:ChromeThickness All=Space.2}</c> or per side
/// (<c>Left=Space.3, Top=Space.1</c>); missing sides are zero, which the token rule allows.
/// </summary>
public sealed class ChromeThicknessExtension : MarkupExtension
{
    public string? Left { get; set; }

    public string? Top { get; set; }

    public string? Right { get; set; }

    public string? Bottom { get; set; }

    /// <summary>All four sides from one token key (a uniform thickness).</summary>
    public string? All
    {
        get => Left;
        set { Left = Top = Right = Bottom = value; }
    }

    public override object ProvideValue(IServiceProvider serviceProvider) => new Thickness(
        Left is null ? 0 : TokenResolver.GetDouble(Left),
        Top is null ? 0 : TokenResolver.GetDouble(Top),
        Right is null ? 0 : TokenResolver.GetDouble(Right),
        Bottom is null ? 0 : TokenResolver.GetDouble(Bottom));
}

internal static class TokenResolver
{
    internal static double GetDouble(string? key)
    {
        if (string.IsNullOrEmpty(key))
        {
            throw new InvalidOperationException("A token key is required (e.g. Space.1).");
        }

        var app = global::Avalonia.Application.Current
            ?? throw new InvalidOperationException("Design tokens require a running Avalonia application.");
        if (!app.TryGetResource(key, global::Avalonia.Styling.ThemeVariant.Light, out var value))
        {
            throw new InvalidOperationException($"Design token '{key}' is not in the application resources.");
        }

        return value switch
        {
            double d => d,
            _ => throw new InvalidOperationException($"Design token '{key}' must be a double."),
        };
    }
}
