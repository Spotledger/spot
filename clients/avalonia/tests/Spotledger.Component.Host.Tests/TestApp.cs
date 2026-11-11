using Avalonia;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Styling;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Component.Host.Tests.TestAppFactory))]

// One headless application per process: a component's ControlTheme lives in the app's resources,
// so the tests that open a component must run in order.
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Component.Host.Tests;

/// <summary>The headless application the component tests run in: the design tokens and nothing else.</summary>
public sealed class TestApp : Application
{
    /// <inheritdoc />
    public override void Initialize()
    {
        RequestedThemeVariant = ThemeVariant.Light;
        SpotledgerTheme.MergeTokenResources(this);
    }
}

/// <summary>Builds the headless app with real text shaping.</summary>
public static class TestAppFactory
{
    /// <summary>The app builder the headless test platform uses.</summary>
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<TestApp>()
            .UseSkia()
            .UseHarfBuzz()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions
            {
                UseHeadlessDrawing = false,
                OverlayPopups = false,
            });
}
