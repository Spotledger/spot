using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Threading;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Grid.Tests.TestAppFactory))]

// One headless Avalonia platform per process: serialize test collections so the
// boot and window creation never race (the Avalonia headless template's default).
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// The headless test application: the same theme stack the Desktop host would install
/// (Spotledger token resources plus the Controls library ControlThemes) and nothing
/// else — the grid tests must pass with tokens resolving, no Fluent theme present.
/// The Grid project paints everything itself, so there is no control theme to load.
/// </summary>
public sealed class TestApp : Application
{
    public override void Initialize()
    {
        SpotledgerTheme.MergeTokenResources(this);
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml"),
        });
    }
}

public static class TestAppFactory
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<TestApp>()
            .UseSkia()
            .UseHarfBuzz()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions
            {
                // The screenshot set needs a real rasterizer (a CaptureRenderedFrame
                // without it returns null — the same shape the Shell tests use);
                // Avalonia.Skia arrives through the Controls Tests' shared restore or
                // the headless package's own dependency graph.
                UseHeadlessDrawing = false,
                OverlayPopups = false,
            });
}

/// <summary>Shared helpers for the grid tests.</summary>
public static class TestHost
{
    /// <summary>Shows content in a window, lays it out and pumps the dispatcher.</summary>
    public static Window Show(Control content, double width = 600, double height = 400)
    {
        var window = new Window { Width = width, Height = height, Content = content };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        return window;
    }

    /// <summary>Pumps dispatcher jobs (window completions arrive on the UI context).</summary>
    public static void Pump() => Dispatcher.UIThread.RunJobs();
}