using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Grid;
using Xunit;

namespace Spotledger.Client.Grid.Tests;

/// <summary>A bare grid in a right-to-left window mirrors its layout but never its glyphs
/// (WP-UI-08.T7): the cell text of the RTL render must be the upright text, not the
/// horizontal mirror image of the left-to-right render.</summary>
public sealed class RtlGlyphTests : IDisposable
{
    private const double Width = 300;
    private const double Height = 120;
    private readonly List<Window> _windows = [];

    public void Dispose()
    {
        foreach (var window in _windows)
        {
            window.Close();
        }
    }

    private async Task<(byte[] Pixels, int Stride)> Render(bool rtl)
    {
        var grid = new SpotGrid
        {
            Columns = GridFixtures.PixelColumns(1, 280),
            Source = new InMemoryGridSource(Enumerable.Range(0, 12).Select(i => new GridRow("k" + i, [new GridCell("Fa7R")]))),
            Width = Width,
            Height = Height,
        };
        if (rtl)
        {
            grid.FlowDirection = FlowDirection.RightToLeft;
        }

        var window = new Window { Width = Width, Height = Height, Content = grid };
        window.Show();
        TestHost.Pump();
        TestHost.Pump();
        _windows.Add(window);

        // Rows arrive from the source off the UI thread: wait until the window is applied.
        for (var i = 0; i < 100 && (grid.VisibleRows is not { Count: > 0 } vr || vr[0] is null); i++)
        {
            await Task.Delay(10, TestContext.Current.CancellationToken);
            TestHost.Pump();
        }

        Assert.True(grid.VisibleRows is { Count: > 0 } rows && rows[0] is not null, "the first row never arrived");
        grid.InvalidateVisual();
        for (var i = 0; i < 3; i++)
        {
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }

        Avalonia.Media.Imaging.WriteableBitmap? frame = null;
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
        return (pixels, stride);
    }

    /// <summary>The grey levels (0 = ink) of the tight bounding box of the dark pixels in
    /// the first body row.</summary>
    private static int[,] GlyphCrop(byte[] pixels, int stride, int top, int bottom)
    {
        int minX = int.MaxValue, maxX = -1, minY = int.MaxValue, maxY = -1;
        bool Dark(int x, int y) => pixels[(y * stride) + (x * 4) + 1] < 110;
        for (var y = top; y < bottom; y++)
        {
            for (var x = 0; x < (int)Width; x++)
            {
                if (Dark(x, y))
                {
                    minX = Math.Min(minX, x);
                    maxX = Math.Max(maxX, x);
                    minY = Math.Min(minY, y);
                    maxY = Math.Max(maxY, y);
                }
            }
        }

        Assert.True(maxX > minX, "no glyph ink found in the first body row");
        var crop = new int[maxX - minX + 1, maxY - minY + 1];
        for (var x = 0; x < crop.GetLength(0); x++)
        {
            for (var y = 0; y < crop.GetLength(1); y++)
            {
                crop[x, y] = pixels[((minY + y) * stride) + ((minX + x) * 4) + 1];
            }
        }

        return crop;
    }

    private static long Distance(int[,] a, int[,] b, bool flipB)
    {
        var w = Math.Min(a.GetLength(0), b.GetLength(0));
        var h = Math.Min(a.GetLength(1), b.GetLength(1));
        long sum = 0;
        for (var x = 0; x < w; x++)
        {
            for (var y = 0; y < h; y++)
            {
                var bx = flipB ? b.GetLength(0) - 1 - x : x;
                sum += Math.Abs(a[x, y] - b[bx, y]);
            }
        }

        return sum;
    }

    [AvaloniaFact]
    public async Task RtlGlyphsAreUprightNotAMirrorOfTheLtrRender()
    {
        var (ltr, ltrStride) = await Render(rtl: false);
        var (rtl, rtlStride) = await Render(rtl: true);

        // The first body row sits just under the header band.
        var headerHeight = (int)Math.Ceiling(new SpotGrid().HeaderHeight);
        var rowHeight = (int)Math.Ceiling(new SpotGrid().RowHeight);
        var ltrCrop = GlyphCrop(ltr, ltrStride, headerHeight + 1, headerHeight + rowHeight - 1);
        var rtlCrop = GlyphCrop(rtl, rtlStride, headerHeight + 1, headerHeight + rowHeight - 1);

        // Layout mirrored: the RTL ink sits right of the LTR ink's position.
        var ltrWidth = ltrCrop.GetLength(0);
        Assert.InRange(rtlCrop.GetLength(0), ltrWidth - 2, ltrWidth + 2);

        var upright = Distance(rtlCrop, ltrCrop, flipB: false);
        var mirrored = Distance(rtlCrop, ltrCrop, flipB: true);
        Assert.True(
            upright < mirrored,
            $"RTL glyphs are a mirror of the LTR render (distance to upright {upright}, to mirror {mirrored})");
    }
}
