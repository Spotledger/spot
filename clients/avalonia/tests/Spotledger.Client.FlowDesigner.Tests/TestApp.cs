using System;
using System.IO;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Threading;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.FlowDesigner.Tests.TestAppFactory))]

// One headless Avalonia platform per process: serialize test collections so boot and window
// creation never race (the same policy the Grid and Graph tests use).
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.FlowDesigner.Tests;

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

/// <summary>Shared helpers: show content in a headless window, lay it out, pump the dispatcher,
/// and wait for a task from the UI thread without deadlocking it.</summary>
public static class TestHost
{
    /// Shows content in a window, lays it out and pumps the dispatcher.
    public static Window Show(Control content, double width = 900, double height = 640)
    {
        Window window = new() { Width = width, Height = height, Content = content };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        return window;
    }
    /// Pumps dispatcher jobs.
    public static void Pump() => Dispatcher.UIThread.RunJobs();
    /// <summary>Waits for a task by pumping the dispatcher, so a continuation posted back to the
    /// UI thread can run.</summary>
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
    /// The directory the test binaries and golden files were copied to.
    public static string OutputDirectory => AppContext.BaseDirectory;
    /// Reads a golden file from the output directory.
    public static string Golden(string name) =>
        File.ReadAllText(Path.Combine(OutputDirectory, "Goldens", name));

    /// <summary>Reads one of the server's shipped BPM templates straight from the repository, so the
    /// test compares the client with the crate's own files rather than a copy.</summary>
    public static string Template(string name)
    {
        DirectoryInfo? directory = new(OutputDirectory);
        while (directory is not null)
        {
            string candidate = Path.Combine(
                directory.FullName, "crates", "framework", "spot-bpm-model", "templates", name);
            if (File.Exists(candidate))
            {
                return File.ReadAllText(candidate);
            }

            directory = directory.Parent;
        }

        throw new FileNotFoundException("The template is not in the repository: " + name);
    }
}
