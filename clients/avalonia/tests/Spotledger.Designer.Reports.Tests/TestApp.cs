using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Themes.Fluent;
using Avalonia.Threading;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Designer.Reports.Tests.TestAppFactory))]

// One headless Avalonia platform per process: serialize test collections.
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Designer.Reports.Tests;

/// <summary>The same theme stack the Desktop host installs: Fluent, the token resources and the Controls library's themes.</summary>
public sealed class TestApp : Application
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

public static class TestAppFactory
{
    // A real rasterizer (not the no-op drawing), so the picture checks see real pixels.
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
    public static Window Show(Control content, double width = 1100, double height = 700)
    {
        var window = new Window { Width = width, Height = height, Content = content };
        window.Show();
        Pump();
        return window;
    }

    public static void Pump() => Dispatcher.UIThread.RunJobs();
}

public static class Pumping
{
    /// <summary>Pumps the UI until the condition holds (up to <paramref name="ms"/>); false when it never did.</summary>
    public static async Task<bool> WaitAsync(Func<bool> condition, int ms = 3000)
    {
        for (var waited = 0; waited < ms; waited += 5)
        {
            TestHost.Pump();
            if (condition())
            {
                return true;
            }

            await Task.Delay(5);
        }

        TestHost.Pump();
        return condition();
    }
}
