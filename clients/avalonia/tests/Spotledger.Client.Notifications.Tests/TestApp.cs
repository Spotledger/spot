using Avalonia;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Notifications.Tests.TestAppFactory))]
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Notifications.Tests;

/// <summary>
/// The headless application the notification views need: the Spotledger token resources and the
/// Controls library themes, rendered by Skia. Without an AvaloniaTestApplication the assembly has
/// no rasterizer, and the accessibility host's CaptureRenderedFrame never returns a frame ("No
/// rendered frame after 20 pumps"), on every runner.
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
    public static AppBuilder BuildAvaloniaApp() => AppBuilder.Configure<TestApp>()
        .UseSkia().UseHarfBuzz().UseHeadless(new AvaloniaHeadlessPlatformOptions { UseHeadlessDrawing = false });
}
