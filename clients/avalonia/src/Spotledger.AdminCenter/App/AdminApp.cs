using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Themes.Fluent;
using Spotledger.Client.Theme;

namespace Spotledger.AdminCenter.App;

/// <summary>
/// Thin host: shows the navigation the console computes. Real tools register in
/// WP-ADM-09.T2 to T4; until then the registry is empty.
/// </summary>
public sealed class AdminApp : Application
{
    public override void Initialize()
    {
        Styles.Add(new FluentTheme());
        SpotledgerTheme.MergeTokenResources(this);
    }

    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            var console = new AdminConsoleViewModel(
                new ToolRegistry(),
                new EquivalentCommand([]),
                new AdminPrincipal("anonymous", new HashSet<AdminRole>()),
                new Entitlements("Community", new HashSet<string>()));
            desktop.MainWindow = new Window
            {
                Title = "Spotledger Admin Center",
                Width = 1024,
                Height = 700,
                Content = new ListBox { ItemsSource = console.Navigation.Select(item => item.Tool.Title).ToList() },
            };
        }

        base.OnFrameworkInitializationCompleted();
    }
}
