using Avalonia;
using Avalonia.Headless;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Themes.Fluent;
using Spotledger.Client.Theme;

// The Avalonia.Headless boot type: it exposes a static BuildAvaloniaApp factory.
[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Shell.Tests.TestAppBuilder))]

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Minimal application the headless view tests run under: the same theme stack the Desktop
/// host installs — Fluent plus the Spotledger token resources — without a real lifetime.
/// </summary>
public sealed class HeadlessTestApplication : Application
{
    public override void Initialize()
    {
        Styles.Add(new FluentTheme());
        SpotledgerTheme.MergeTokenResources(this);
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml"),
        });
    }
}

/// <summary>The Avalonia.Headless entry point: a static BuildAvaloniaApp the framework boots.</summary>
public static class TestAppBuilder
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<HeadlessTestApplication>()
            .UseSkia()
            .UseHarfBuzz()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions
            {
                // The screenshot harness (WP-UI-02.T2) needs a real rasterizer: Avalonia.Skia
                // arrives transitively through the Desktop project reference. Popups stay
                // embedded in the window (the default) so captured frames include open menus.
                UseHeadlessDrawing = false,
            });
}
