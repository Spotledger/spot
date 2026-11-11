using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Threading;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Forms.Tests.TestAppFactory))]

// One headless Avalonia platform per process: serialize test collections.
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Forms.Tests;

/// <summary>The same theme stack the Desktop host installs: token resources plus the Controls
/// library's control themes. The grid paints itself and needs nothing else.</summary>
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
                UseHeadlessDrawing = false,
                OverlayPopups = false,
            });
}

public static class TestHost
{
    public static Window Show(Control content, double width = 900, double height = 500)
    {
        var window = new Window { Width = width, Height = height, Content = content };
        window.Show();
        Pump();
        return window;
    }

    public static void Pump() => Dispatcher.UIThread.RunJobs();
}
