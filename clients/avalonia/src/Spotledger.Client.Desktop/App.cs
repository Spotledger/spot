using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Styling;
using Avalonia.Themes.Fluent;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Desktop;

public sealed class App : Application
{
    internal static bool ShowTokenGallery { get; set; }

    public override void Initialize()
    {
        Styles.Add(new FluentTheme());
        if (ShowTokenGallery)
        {
            // Only the gallery needs a deterministic start theme; the main window keeps
            // following the OS preference.
            RequestedThemeVariant = ThemeVariant.Light;
        }

        SpotledgerTheme.MergeTokenResources(this);

        // The field controls of a document form (WP-UI-17.T4 hosts it): without their control themes
        // a form would show unstyled, empty field wrappers.
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml"),
        });
    }

    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is IClassicDesktopStyleApplicationLifetime desktop)
        {
            desktop.MainWindow = ShowTokenGallery ? new TokenGalleryWindow() : new MainWindow();
        }

        base.OnFrameworkInitializationCompleted();
    }
}
