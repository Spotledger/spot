using System.Reflection;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Theme;
using Xunit;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Client.Controls.Tests.TestAppFactory))]
[assembly: CollectionBehavior(DisableTestParallelization = true)]

namespace Spotledger.Client.Controls.Tests;

/// <summary>
/// The headless test application: Theme tokens plus the library ControlThemes, and nothing
/// else — the gallery and behaviour tests must pass with no Fluent theme present, which is
/// what proves <c>Controls.axaml</c> is self-contained.
/// </summary>
public sealed class TestApp : Application
{
    public override void Initialize()
    {
        System.Runtime.CompilerServices.RuntimeHelpers.RunClassConstructor(typeof(Spotledger.Client.Controls.ControlGallery).TypeHandle);
        SpotledgerTheme.MergeTokenResources(this);
        Resources.MergedDictionaries.Add(new ResourceInclude(new System.Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new System.Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml")
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
                // Platform popups (not overlay layer): Popup.Open then creates a real
                // headless popup top level, matching Avalonia's own headless test setup.
                UseHeadlessDrawing = false,
                OverlayPopups = false
            });
}

/// <summary>Shared helpers for the control tests.</summary>
internal static class TestHost
{
    /// <summary>Shows <paramref name="content"/> in a window and lays it out.</summary>
    public static Window Show(Control content, double width = 600, double height = 400)
    {
        var window = new Window { Width = width, Height = height, Content = content };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        return window;
    }

    /// <summary>
    /// Moves focus from <paramref name="from"/> to a fresh sibling so its LostFocus commit
    /// path runs — Avalonia 12's IInputRoot.FocusManager is internal API, so the focus
    /// hand-off is done through public control focus instead.
    /// </summary>
    public static void ClearFocus(Control from)
    {
        var sibling = new Button { Content = "rival" };
        var panel = from.GetVisualParent() as Panel
            ?? throw new InvalidOperationException("The control must live in a Panel for the focus rival.");
        panel.Children.Add(sibling);
        sibling.Focus();
        Dispatcher.UIThread.RunJobs();
    }
}
