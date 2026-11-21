using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Input.Raw;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Demo.Mock;

[assembly: AvaloniaTestApplication(typeof(Spotledger.Demo.Tests.TestAppFactory))]

namespace Spotledger.Demo.Tests;

public static class TestAppFactory
{
    public static AppBuilder BuildAvaloniaApp() => AppBuilder.Configure<DemoApp>()
        .UseHeadless(new AvaloniaHeadlessPlatformOptions());
}
internal static class TestHost
{
    internal static async Task<(Window Window, ShellView View, ShellViewModel Model)> ShowAsync()
    {
        var model = new ShellViewModel(new MockBackend()); await model.InitializeAsync();
        var view = new ShellView(model);
        var window = new Window { Content = view, Width = 1440, Height = 1000 };
        window.Show(); Pump(); return (window, view, model);
    }
    internal static void Pump() => Dispatcher.UIThread.RunJobs();
    internal static T Find<T>(ShellView view, string name) where T : Control
        => view.GetVisualDescendants().OfType<T>().Single(c => c.Name == name);
    internal static void Click(ShellView view, string name)
        => Find<SpotButton>(view, name).Command!.Execute(null);
    internal static void Key(Window window, Key key, KeyModifiers modifiers = KeyModifiers.None)
    {
        var raw = RawInputModifiers.None;
        if (modifiers.HasFlag(KeyModifiers.Control)) raw |= RawInputModifiers.Control;
        if (modifiers.HasFlag(KeyModifiers.Shift)) raw |= RawInputModifiers.Shift;
        if (modifiers.HasFlag(KeyModifiers.Alt)) raw |= RawInputModifiers.Alt;
        window.KeyPress(key, raw, PhysicalKey.None, null); window.KeyRelease(key, raw, PhysicalKey.None, null); Pump();
    }
}
