using System;
using System.Threading;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Threading;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.FlowDesigner.Run.Tests.TestAppFactory))]

// One headless Avalonia platform per process: serialize test collections so boot and window
// creation never race (the same policy the designer, Grid and Graph tests use).
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The headless test application: the same theme stack the Desktop host installs.
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

/// Shared helpers: show content in a headless window, lay it out, pump the dispatcher, and wait for
/// a task from the UI thread without deadlocking it.
public static class TestHost
{

    /// Shows content in a window, lays it out and pumps the dispatcher.
    public static Window Show(Control content, double width = 1024, double height = 700)
    {
        Window window = new() { Width = width, Height = height, Content = content };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        return window;
    }

    /// Pumps dispatcher jobs.
    public static void Pump() => Dispatcher.UIThread.RunJobs();

    /// Waits for a task by pumping the dispatcher, so a continuation posted back to the UI thread
    /// can run.
    public static void Await(System.Threading.Tasks.Task task)
    {
        ArgumentNullException.ThrowIfNull(task);
        for (int pump = 0; pump < 5_000_000 && !task.IsCompleted; pump++)
        {
            Dispatcher.UIThread.RunJobs();
            Thread.Sleep(0);
        }

        if (!task.IsCompleted)
        {
            throw new InvalidOperationException("The awaited task did not complete.");
        }

        task.GetAwaiter().GetResult();
    }

    /// Waits for a task and answers its result.
    public static T Await<T>(System.Threading.Tasks.Task<T> task)
    {
        ArgumentNullException.ThrowIfNull(task);
        Await((System.Threading.Tasks.Task)task);
        return task.GetAwaiter().GetResult();
    }
}
