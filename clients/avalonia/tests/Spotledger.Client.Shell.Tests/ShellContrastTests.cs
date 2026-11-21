using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Immutable;
using Avalonia.Styling;
using Avalonia.Threading;
using Spotledger.Client.Api;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// WCAG contrast of the signed-in shell's text against the surface it actually sits on,
/// sampled from the RESOLVED brushes in the rendered visual tree (never from pixels, and
/// independent of any screenshot baseline).
///
/// This is the gate <see cref="ShellRtlMatrixTests"/>'s pixel baselines could not be: a baseline
/// passes once it is re-recorded, even from a broken render — which is exactly what WP-UI-15.T8
/// did to the Dark chrome (heading, body text and inactive tab labels were dark-on-dark and
/// nearly invisible, and the baseline comparison never noticed because it was recorded from that
/// same broken frame). A resolved-colour contrast assertion fails on the actual colours no
/// matter what picture a reviewer approved, so it keeps failing until the colours are actually
/// legible. Light and HighContrastDark run the identical assertion, so a regression in either
/// direction — not only Dark — is caught.
///
/// <see cref="ContrastRatio"/>/<see cref="RelativeLuminance"/> are the plain WCAG 2 formulas
/// (ordinary public arithmetic, not Spotledger or ERPNext source). An equivalent helper already
/// exists for the token *source* in
/// <c>Spotledger.Client.Theme.Tests/TokenContractTests.cs</c> (its private
/// <c>ContrastRatio</c>/<c>RelativeLuminance</c>, checked against <c>design/tokens.json</c>);
/// it is private to another test assembly and checks the token file, not a live render, so this
/// test cannot reference it and keeps its own small copy of the same standard formula.
/// </summary>
public sealed class ShellContrastTests
{
    private const double NormalTextMinimumRatio = 4.5;
    private const double LargeTextMinimumRatio = 3.0;

    private static async Task<(ShellView View, ApiConnection Connection)> BuildShellAsync()
    {
        SpotledgerFonts.Register();
        if (Application.Current is { } application)
        {
            application.Resources["Font.Family.Latin"] = SpotledgerFonts.Latin;
        }

        var connection = new ApiConnection(new FakeServerHandler());
        var shell = new ShellViewModel(connection);
        await shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        connection.Connect(new Uri(TestHarness.ServerUrl));
        connection.Tokens.SetToken(TestHarness.SessionToken);
        await shell.InitializeAsync(TestContext.Current.CancellationToken);
        await shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);
        shell.Tabs.Add(new WorkspaceTabViewModel("Selling") { IsPinned = true });
        shell.SelectedTab = shell.Tabs[1]; // "Buying" becomes the active tab/heading.
        var view = new ShellView { DataContext = shell, FontFamily = SpotledgerFonts.Latin };
        return (view, connection);
    }

    private static void Pump()
    {
        for (var round = 0; round < 5; round++)
        {
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light)]
    [InlineData(ThemeVariantKind.Dark)]
    [InlineData(ThemeVariantKind.HighContrastDark)]
    public async Task TheShellsChromeTextMeetsWcagContrastOnItsOwnSurface(ThemeVariantKind variant)
    {
        var application = Application.Current!;
        var previousVariant = application.RequestedThemeVariant;
        var (view, connection) = await BuildShellAsync();
        var requested = variant.ToThemeVariant();
        application.RequestedThemeVariant = requested;
        var window = new Window
        {
            Width = 960,
            Height = 600,
            CanResize = false,
            WindowDecorations = WindowDecorations.None,
            RequestedThemeVariant = requested,
            Content = view,
        };
        try
        {
            window.Show();
            Pump();
            window.UpdateLayout();
            Pump();

            var canvas = ResolveColor(view.Background);
            var content = Assert.IsType<StackPanel>(view.TabContentControl.Content);
            var heading = content.Children.OfType<TextBlock>().ElementAt(0);
            var body = content.Children.OfType<TextBlock>().ElementAt(1);
            AssertRatio($"{variant} heading", ResolveColor(heading.Foreground), canvas, LargeTextMinimumRatio);
            AssertRatio($"{variant} body", ResolveColor(body.Foreground), canvas, NormalTextMinimumRatio);

            var chips = view.TabStripControl.Items.OfType<Border>().ToList();
            var activeChip = chips.First(chip => chip.Classes.Contains("active"));
            var inactiveChip = chips.First(chip => !chip.Classes.Contains("active"));
            AssertRatio($"{variant} active tab label", LabelColor(activeChip), ResolveColor(activeChip.Background), NormalTextMinimumRatio);
            AssertRatio($"{variant} inactive tab label", LabelColor(inactiveChip), ResolveColor(inactiveChip.Background), NormalTextMinimumRatio);
        }
        finally
        {
            window.Content = null;
            window.Close();
            application.RequestedThemeVariant = previousVariant;
            connection.Dispose();
        }
    }

    private static Color LabelColor(Border chip)
    {
        var grid = Assert.IsType<Avalonia.Controls.Grid>(chip.Child);
        var label = grid.Children.OfType<TextBlock>().First();
        return ResolveColor(label.Foreground);
    }

    private static void AssertRatio(string label, Color foreground, Color background, double minimum)
    {
        var ratio = ContrastRatio(foreground, background);
        Assert.True(ratio >= minimum,
            $"{label}: {foreground} on {background} is {ratio:0.00}:1, needs at least {minimum:0.0}:1.");
    }

    private static Color ResolveColor(IBrush? brush) => brush switch
    {
        SolidColorBrush solid => solid.Color,
        ImmutableSolidColorBrush immutable => immutable.Color,
        _ => throw new InvalidOperationException($"Expected a resolved solid colour brush but got {brush?.GetType().Name ?? "null"}."),
    };

    // ---- WCAG 2 contrast (standard formula; https://www.w3.org/TR/WCAG20/#relativeluminancedef) ----

    private static double ContrastRatio(Color foreground, Color background)
    {
        var lighter = Math.Max(RelativeLuminance(foreground), RelativeLuminance(background));
        var darker = Math.Min(RelativeLuminance(foreground), RelativeLuminance(background));
        return (lighter + 0.05) / (darker + 0.05);
    }

    private static double RelativeLuminance(Color color)
    {
        double Channel(byte value)
        {
            var normalized = value / 255.0;
            return normalized <= 0.03928 ? normalized / 12.92 : Math.Pow((normalized + 0.055) / 1.055, 2.4);
        }

        return 0.2126 * Channel(color.R) + 0.7152 * Channel(color.G) + 0.0722 * Channel(color.B);
    }
}
