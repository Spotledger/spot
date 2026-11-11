using System.Runtime.CompilerServices;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Extensions.Tests.TestAppFactory))]
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Extensions.Tests;

/// <summary>
/// The headless test application: theme tokens plus the library's ControlThemes, with Skia
/// and HarfBuzz so <c>CaptureRenderedFrame</c> returns real pixels and text lays out with
/// the same font stack the desktop head uses. Plain <c>[Fact]</c> does not boot Avalonia;
/// every test that touches a control uses <c>[AvaloniaFact]</c>.
/// </summary>
public sealed class TestApp : Application
{
    public override void Initialize()
    {
        RuntimeHelpers.RunClassConstructor(typeof(ControlGallery).TypeHandle);
        SpotledgerTheme.MergeTokenResources(this);
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml")
        });
    }
}

public static class TestAppFactory
{
    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<TestApp>()
            .UseSkia()
            .UseHarfBuzz()
            .UseHeadless(new AvaloniaHeadlessPlatformOptions { UseHeadlessDrawing = false, OverlayPopups = false });
}

/// <summary>Small helpers shared by the renderer tests.</summary>
internal static class TestHost
{
    /// <summary>Shows <paramref name="content"/> in a window and runs a layout pass.</summary>
    public static Window Show(Control content, double width = 420, double height = 360, bool rtl = false, bool dark = false)
    {
        Window window = new()
        {
            Width = width,
            Height = height,
            Content = content,
            FlowDirection = rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight,
            RequestedThemeVariant = dark ? Avalonia.Styling.ThemeVariant.Dark : Avalonia.Styling.ThemeVariant.Light
        };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        return window;
    }
}
