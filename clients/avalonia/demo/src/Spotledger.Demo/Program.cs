using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Styling;
using Avalonia.Themes.Fluent;
using Spotledger.Client.Theme;
using Spotledger.Demo.Mock;

namespace Spotledger.Demo;

internal static class Program
{
    [STAThread]
    public static void Main(string[] args) => BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);
    public static AppBuilder BuildAvaloniaApp() => AppBuilder.Configure<DemoApp>().UsePlatformDetect();
}
public sealed class DemoApp : Application
{
    public override void Initialize()
    {
        RequestedThemeVariant = ThemeVariant.Light;
        Styles.Add(new FluentTheme());
        SpotledgerTheme.MergeTokenResources(this);
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml")
        });
    }
    public override async void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            var model = new ShellViewModel(new MockBackend());
            await model.InitializeAsync();
            var view = new ShellView(model);
            var window = new Window { Title = "Spotledger · Demo", Content = view, WindowState = WindowState.Maximized };
            // OS close goes through the same dirty prompt as File → Exit.
            var confirmed = false;
            model.ExitRequested += () => { confirmed = true; window.Close(); };
            window.Closing += (sender, e) =>
            {
                if (confirmed) return;
                e.Cancel = true;
                _ = model.RunAsync("shell.exit", CommandRoute.Menu);
            };
            desktop.MainWindow = window;
        }
        base.OnFrameworkInitializationCompleted();
    }
}
