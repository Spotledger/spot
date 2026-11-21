using Avalonia;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Component.Tests.TestAppFactory))]

// One headless application per process: the contract kit merges ControlThemes into the app's
// resources, so tests must not race each other over them.
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Component.Tests;

/// <summary>
/// The headless application the contract kit runs in: the Theme tokens and nothing else. That is
/// deliberate — a component that needs application styles to look right is not a component, it is
/// part of the app.
/// </summary>
public sealed class TestApp : Application
{
    public override void Initialize()
    {
        RequestedThemeVariant = ThemeVariant.Light;
        SpotledgerTheme.MergeTokenResources(this);
    }
}

/// <summary>Builds the headless app with real text shaping, which the Urdu checks depend on.</summary>
public static class TestAppFactory
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<TestApp>()
            .UseSkia()
            .UseHarfBuzz()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions
            {
                UseHeadlessDrawing = false,
                OverlayPopups = false
            });
}
