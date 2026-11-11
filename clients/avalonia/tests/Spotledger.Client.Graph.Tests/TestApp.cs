using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Threading;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Graph.Tests.TestAppFactory))]

// One headless Avalonia platform per process: serialize test collections so the boot and
// window creation never race (the same policy the Grid tests use).
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Graph.Tests;

/// <summary>The headless test application: the same theme stack the Desktop host installs
/// (Spotledger token resources plus the Controls library themes). The graph views paint
/// themselves, so there is no control theme of their own to load.</summary>
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

/// <summary>Shared helpers: show content in a headless window, lay it out, pump the
/// dispatcher, and synthesize key presses through the real input pipeline.</summary>
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

    /// <summary>Pumps dispatcher jobs (page completions arrive on the UI context).</summary>
    public static void Pump() => Dispatcher.UIThread.RunJobs();

    /// <summary>Waits for a task from the UI thread by pumping the dispatcher, so a
    /// continuation posted back to the UI thread can run (blocking with GetResult would
    /// deadlock the headless platform).</summary>
    /// <param name="task">The task to wait for.</param>
    public static void Await(Task task)
    {
        ArgumentNullException.ThrowIfNull(task);
        for (var pump = 0; pump < 200_000 && !task.IsCompleted; pump++)
        {
            Dispatcher.UIThread.RunJobs();
            Thread.Sleep(0);
        }

        task.GetAwaiter().GetResult();
    }
}
