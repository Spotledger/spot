using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// The screenshot set (brief §7): normal and compact density × light and dark × 3
/// widths. Every capture writes ONLY inside this test project's own output folder —
/// never into docs/ — so ordinary test runs cannot overwrite committed baselines.
/// </summary>
public sealed class GridScreenshotTests : IDisposable
{
    /// <summary>The captures land in the tests' own bin folder, inside the owned paths.</summary>
    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "grid");
        Directory.CreateDirectory(dir);
        return dir;
    }

    private readonly List<Window> _windows = [];
    private ThemeVariant _previousVariant = ThemeVariant.Light;

    private Window MakeGridWindow(double width, double height, DensityMode density, ThemeVariant variant)
    {
        var app = Application.Current!;
        _previousVariant = app.RequestedThemeVariant ?? ThemeVariant.Light;
        app.RequestedThemeVariant = variant;
        TestHost.Pump();

        var columns = new List<GridColumnModel>
        {
            new() { Header = "Product", Width = GridColumnWidth.Pixels(120) },
            new() { Header = "Qty", Width = GridColumnWidth.Pixels(70), DefaultAlignment = GridCellAlignment.End },
            new() { Header = "Unit price", Width = GridColumnWidth.Star(1), DefaultAlignment = GridCellAlignment.End },
            new() { Header = "Note", Width = GridColumnWidth.Star(2) },
        };

        var source = new InMemoryGridSource(GridFixture.MakeRows(60, 4));
        var grid = new SpotGrid
        {
            Columns = columns,
            Source = source,
            Footer = GridRow.Of("footer", new GridCell("Totals"), new GridCell(""), new GridCell("1,240.00"), new GridCell("")),
            Width = width,
            Height = height,
        };
        Density.SetMode(grid, density);

        var window = new Window { Width = width, Height = height, Content = grid };
        window.Show();
        TestHost.Pump();
        TestHost.Pump();
        _windows.Add(window);
        return window;
    }

    private static void Capture(Window window, string name)
    {
        Bitmap? frame = null;
        for (var attempt = 0; attempt < 40 && frame is null; attempt++)
        {
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            frame = window.CaptureRenderedFrame();
        }

        Assert.NotNull(frame);
        var path = Path.Combine(OutputDirectory(), name);
        using var stream = File.Create(path);
        frame.Save(stream, new PngBitmapEncoderOptions());
    }

    [AvaloniaTheory]
    [InlineData(1, 1280)]
    [InlineData(2, 960)]
    [InlineData(3, 640)]
    public void CaptureComfortableLightAtThreeWidths(int index, double width)
    {
        var window = MakeGridWindow(width, 480, DensityMode.Comfortable, ThemeVariant.Light);
        Capture(window, $"grid-comfortable-light-{index}-{(int)width}.png");
    }

    [AvaloniaTheory]
    [InlineData(1, 1280)]
    [InlineData(2, 960)]
    [InlineData(3, 640)]
    public void CaptureCompactDarkAtThreeWidths(int index, double width)
    {
        var window = MakeGridWindow(width, 480, DensityMode.Compact, ThemeVariant.Dark);
        Capture(window, $"grid-compact-dark-{index}-{(int)width}.png");
    }

    public void Dispose()
    {
        foreach (var window in _windows)
        {
            window.Close();
        }

        if (Application.Current is { } app)
        {
            app.RequestedThemeVariant = _previousVariant;
        }

        TestHost.Pump();
    }
}