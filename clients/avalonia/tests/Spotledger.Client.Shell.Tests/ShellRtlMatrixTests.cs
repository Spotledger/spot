using System.IO;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Threading;
using Spotledger.Client.Api;
using Spotledger.Client.Controls;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.TestSupport;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The shell chrome in the right-to-left matrix (WP-UI-15.T8). The same signed-in shell the
/// screenshot parity test builds (the fake server, a few tabs) is rendered at a fixed small size,
/// right-to-left, in Light and Dark at both densities plus the high-contrast palette in Comfortable,
/// and compared with committed baselines through <see cref="ScreenshotAssert"/>. This replaces the
/// one-off <c>07-rtl.png</c> capture, which nothing compared with anything.
///
/// Determinism: the shell shows no clock and no version (the session header is constants from the
/// fake server), and the Latin face is the bundled Noto Sans, so a baseline is a picture of our
/// layout and not of the runner's font package. Regenerate with
/// <c>SPOT_UPDATE_SHOTS=1 dotnet test clients/avalonia/tests/Spotledger.Client.Shell.Tests --filter FullyQualifiedName~ShellRtlMatrixTests</c>,
/// review every image, and commit.
/// </summary>
public sealed class ShellRtlMatrixTests
{
    private const int Width = 960;
    private const int Height = 600;

    private static readonly string Baselines =
        Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "Baselines"));

    private static ScreenshotMatrix Case(ThemeVariantKind variant, DensityMode density) =>
        new(Baselines, Width, Height, density, dark: variant.IsDark(), rtl: true, Variant: variant);

    /// <summary>Builds the signed-in shell over the fake server with a few tabs, right-to-left.</summary>
    private static async Task<(ShellView View, ApiConnection Connection)> BuildShellAsync(bool compact = false)
    {
        // The bundled faces, registered the way the theme host does; the product token prefers
        // fonts a Linux runner may not have.
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
        shell.NotificationCount = 3;
        await shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);
        shell.OpenDocumentTab("Buying", Guid.Parse("8f14e45f-e1b1-4b5a-9b0e-1c2d3e4f5a6b"),
            "Purchase Order · PO-1042", "PO-1042").IsDirty = true;
        shell.Tabs.Add(new WorkspaceTabViewModel("Selling") { IsPinned = true });
        shell.SelectedTab = shell.Tabs[1];
        shell.RightToLeft = true;
        shell.CompactDensity = compact;
        var view = new ShellView { DataContext = shell, FontFamily = SpotledgerFonts.Latin };
        return (view, connection);
    }

    private static async Task AssertMatrixCaseAsync(ThemeVariantKind variant, DensityMode density)
    {
        var (view, connection) = await BuildShellAsync(density == DensityMode.Compact);
        try
        {
            var window = ScreenshotAssert.MatchesWindow("shell-chrome", view, Case(variant, density));
            try
            {
                Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
                Assert.Equal(density, Density.GetMode(view));
            }
            finally
            {
                ScreenshotAssert.Close(window);
            }
        }
        finally
        {
            connection.Dispose();
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Light, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.HighContrast, DensityMode.Comfortable)]
    public async Task TheShellChromeMatchesItsRightToLeftBaseline(ThemeVariantKind variant, DensityMode density) =>
        await AssertMatrixCaseAsync(variant, density);

    [AvaloniaFact]
    public async Task TwoCapturesInOneProcessAreByteIdentical()
    {
        var first = await CaptureBytesAsync();
        var second = await CaptureBytesAsync();
        Assert.True(first.AsSpan().SequenceEqual(second), "the shell chrome renders differently on a second capture");
    }

    private static async Task<byte[]> CaptureBytesAsync()
    {
        var (view, connection) = await BuildShellAsync();
        var window = new Window
        {
            Width = Width,
            Height = Height,
            CanResize = false,
            FlowDirection = FlowDirection.RightToLeft,
            Content = view,
        };
        try
        {
            window.Show();
            Dispatcher.UIThread.RunJobs();
            using var frame = ScreenshotAssert.Capture(window);
            using var stream = new MemoryStream();
            frame.Save(stream, new PngBitmapEncoderOptions());
            return stream.ToArray();
        }
        finally
        {
            ScreenshotAssert.Close(window);
            connection.Dispose();
        }
    }
}
