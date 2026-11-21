using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>Annotation drawing (WP-UI-08.T5): the screenshot set (normal and compact,
/// light and dark, RTL), the guarantee that an unannotated grid draws exactly as before
/// and that an annotation changes only its marker corner, and that the three severities
/// are told apart by shape.</summary>
public sealed class AnnotationDrawingTests : IDisposable
{
    private const double Width = 420;
    private const double Height = 260;

    private readonly List<Window> _windows = [];
    private ThemeVariant _previousVariant = ThemeVariant.Light;

    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "grid-annotations");
        Directory.CreateDirectory(dir);
        return dir;
    }

    private static List<GridRow> Rows(Func<int, int, GridCellAnnotation?> note, Func<int, GridCellAnnotation?>? rowNote = null)
    {
        var rows = new List<GridRow>();
        for (var r = 0; r < 8; r++)
        {
            var cells = new GridCell[3];
            for (var c = 0; c < 3; c++)
            {
                cells[c] = new GridCell($"r{r}c{c}") { Annotation = note(r, c) };
            }

            rows.Add(new GridRow($"k{r}", cells) { RowAnnotation = rowNote?.Invoke(r) });
        }

        return rows;
    }

    private (Window Window, SpotGrid Grid) Show(
        List<GridRow> rows,
        DensityMode density = DensityMode.Comfortable,
        ThemeVariant? variant = null,
        bool rtl = false)
    {
        var app = Application.Current!;
        _previousVariant = app.RequestedThemeVariant ?? ThemeVariant.Light;
        app.RequestedThemeVariant = variant ?? ThemeVariant.Light;
        TestHost.Pump();

        var source = new InMemoryGridSource(rows);
        var grid = new SpotGrid
        {
            Columns = GridFixtures.PixelColumns(3, 120),
            Source = source,
            Width = Width,
            Height = Height,
        };
        Density.SetMode(grid, density);
        if (rtl)
        {
            grid.FlowDirection = FlowDirection.RightToLeft;
        }

        var window = new Window { Width = Width, Height = Height, Content = grid };
        window.Show();
        TestHost.Pump();
        TestHost.Pump();
        _windows.Add(window);
        return (window, grid);
    }

    private static (WriteableBitmap Frame, byte[] Pixels, int Stride) Capture(Window window)
    {
        WriteableBitmap? frame = null;
        for (var attempt = 0; attempt < 40 && frame is null; attempt++)
        {
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            frame = window.CaptureRenderedFrame();
        }

        Assert.NotNull(frame);
        using var locked = frame.Lock();
        var stride = locked.RowBytes;
        var pixels = new byte[stride * locked.Size.Height];
        Marshal.Copy(locked.Address, pixels, 0, pixels.Length);
        return (frame, pixels, stride);
    }

    private static void Save(WriteableBitmap frame, string name)
    {
        using var stream = File.Create(Path.Combine(OutputDirectory(), name));
        frame.Save(stream, new PngBitmapEncoderOptions());
    }

    /// <summary>Pixel coordinates (x, y) whose bytes differ between two captures.</summary>
    private static List<(int X, int Y)> Differences(byte[] a, byte[] b, int stride)
    {
        Assert.Equal(a.Length, b.Length);
        var result = new List<(int, int)>();
        for (var i = 0; i < a.Length; i += 4)
        {
            if (a[i] != b[i] || a[i + 1] != b[i + 1] || a[i + 2] != b[i + 2] || a[i + 3] != b[i + 3])
            {
                result.Add(((i % stride) / 4, i / stride));
            }
        }

        return result;
    }

    [AvaloniaFact]
    public void NoAnnotationDrawsIdenticallyAcrossRenders()
    {
        var (window, _) = Show(Rows((_, _) => null));
        var first = Capture(window).Pixels;
        window.InvalidateVisual();
        var second = Capture(window);
        Assert.Empty(Differences(first, second.Pixels, second.Stride));
    }

    [AvaloniaTheory]
    [InlineData(GridAnnotationSeverity.Error)]
    [InlineData(GridAnnotationSeverity.Warning)]
    [InlineData(GridAnnotationSeverity.Info)]
    public void CellAnnotationChangesOnlyItsTrailingTopCorner(GridAnnotationSeverity severity)
    {
        var (plainWindow, _) = Show(Rows((_, _) => null));
        var plain = Capture(plainWindow);

        var (window, grid) = Show(Rows((r, c) => r == 2 && c == 1 ? new GridCellAnnotation(severity, "why") : null));
        var marked = Capture(window);

        var diffs = Differences(plain.Pixels, marked.Pixels, plain.Stride);
        Assert.NotEmpty(diffs);

        var cellRight = grid.ColumnX(1) + 120;
        var cellTop = grid.RowY(2);
        foreach (var (x, y) in diffs)
        {
            Assert.InRange(x, cellRight - 9, cellRight);
            Assert.InRange(y, cellTop, cellTop + 9);
        }
    }

    [AvaloniaFact]
    public void SeveritiesDifferInShapeNotOnlyColour()
    {
        var shapes = new Dictionary<GridAnnotationSeverity, HashSet<(int, int)>>();
        var plainWindow = Show(Rows((_, _) => null)).Window;
        var plain = Capture(plainWindow);
        foreach (var severity in Enum.GetValues<GridAnnotationSeverity>())
        {
            var (window, _) = Show(Rows((r, c) => r == 2 && c == 1 ? new GridCellAnnotation(severity, "why") : null));
            var marked = Capture(window);
            shapes[severity] = [.. Differences(plain.Pixels, marked.Pixels, plain.Stride)];
        }

        Assert.NotEqual(shapes[GridAnnotationSeverity.Error], shapes[GridAnnotationSeverity.Warning]);
        Assert.NotEqual(shapes[GridAnnotationSeverity.Error], shapes[GridAnnotationSeverity.Info]);
        Assert.NotEqual(shapes[GridAnnotationSeverity.Warning], shapes[GridAnnotationSeverity.Info]);
    }

    [AvaloniaFact]
    public void RowAnnotationDrawsInTheFirstColumnLeadingBottomCorner()
    {
        var (plainWindow, _) = Show(Rows((_, _) => null));
        var plain = Capture(plainWindow);

        var (window, grid) = Show(
            Rows((_, _) => null, r => r == 3 ? new GridCellAnnotation(GridAnnotationSeverity.Warning, "row") : null));
        var marked = Capture(window);

        var diffs = Differences(plain.Pixels, marked.Pixels, plain.Stride);
        Assert.NotEmpty(diffs);
        var bottom = grid.RowY(3) + grid.RowHeight;
        foreach (var (x, y) in diffs)
        {
            Assert.InRange(x, 0, 9);
            Assert.InRange(y, bottom - 9, bottom);
        }
    }

    [AvaloniaFact]
    public void RtlMirrorsTheCellMarkerToTheTrailingCorner()
    {
        var (plainWindow, _) = Show(Rows((_, _) => null), rtl: true);
        var plain = Capture(plainWindow);

        var (window, grid) = Show(
            Rows((r, c) => r == 2 && c == 1 ? new GridCellAnnotation(GridAnnotationSeverity.Error, "why") : null),
            rtl: true);
        var marked = Capture(window);

        var diffs = Differences(plain.Pixels, marked.Pixels, plain.Stride);
        Assert.NotEmpty(diffs);
        // The flow direction mirrors the grid, so the cell's trailing (right) corner in the
        // grid's own space lands on the mirrored cell's left edge on screen.
        var mirroredLeft = Width - (grid.ColumnX(1) + 120);
        foreach (var (x, y) in diffs)
        {
            Assert.InRange(y, grid.RowY(2), grid.RowY(2) + 9);
            Assert.InRange(x, mirroredLeft, mirroredLeft + 9);
        }
    }

    [AvaloniaTheory]
    [InlineData(DensityMode.Comfortable, false, false)]
    [InlineData(DensityMode.Comfortable, true, false)]
    [InlineData(DensityMode.Compact, false, false)]
    [InlineData(DensityMode.Compact, true, false)]
    [InlineData(DensityMode.Comfortable, false, true)]
    [InlineData(DensityMode.Compact, true, true)]
    public void CaptureTheScreenshotSet(DensityMode density, bool dark, bool rtl)
    {
        GridCellAnnotation? Note(int r, int c) => (r, c) switch
        {
            (1, 1) => new GridCellAnnotation(GridAnnotationSeverity.Error, "error"),
            (3, 2) => new GridCellAnnotation(GridAnnotationSeverity.Warning, "warning"),
            (5, 0) => new GridCellAnnotation(GridAnnotationSeverity.Info, "info"),
            _ => null,
        };

        var (window, _) = Show(
            Rows(Note, r => r == 6 ? new GridCellAnnotation(GridAnnotationSeverity.Error, "row") : null),
            density,
            dark ? ThemeVariant.Dark : ThemeVariant.Light,
            rtl);
        var frame = Capture(window).Frame;
        Save(frame, $"annotations-{density.ToString().ToLowerInvariant()}-{(dark ? "dark" : "light")}{(rtl ? "-rtl" : string.Empty)}.png");
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
