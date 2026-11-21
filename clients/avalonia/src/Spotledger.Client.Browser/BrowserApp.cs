using Avalonia;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Themes.Fluent;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Browser;

/// <summary>
/// The browser application: the same theme and token resources as the desktop app, one view instead of
/// a window. It starts only the minimal shell (server picker, login, home) and no module bundle.
/// </summary>
public sealed class BrowserApp : Application
{
    /// <summary>The composed head state, set by <c>Program</c> before the application starts.</summary>
    internal static BrowserHostState? State { get; set; }

    /// <inheritdoc />
    public override void Initialize()
    {
        Styles.Add(new FluentTheme());
        SpotledgerTheme.MergeTokenResources(this);

        // The field controls of a document form; without them a form shows empty wrappers.
        Resources.MergedDictionaries.Add(new ResourceInclude(new Uri("avares://Spotledger.Client.Controls/"))
        {
            Source = new Uri("avares://Spotledger.Client.Controls/Themes/Controls.axaml"),
        });
    }

    /// <inheritdoc />
    public override void OnFrameworkInitializationCompleted()
    {
        if (ApplicationLifetime is ISingleViewApplicationLifetime single && State is { } state)
        {
            single.MainView = new BrowserShellHost(state);
        }

        base.OnFrameworkInitializationCompleted();
    }
}
