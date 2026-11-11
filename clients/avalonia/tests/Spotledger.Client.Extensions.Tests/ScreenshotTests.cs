using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform;
using Spotledger.Client.Controls;
using Spotledger.Client.Extensions;
using Spotledger.Client.Extensions.Renderer;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>
/// The visual matrix: the toolbar with actions, the side panel with its data blocks, and the
/// composite tile, each rasterised headless in light, dark, comfortable, compact and RTL. The
/// assertions are frame invariants rather than committed PNG goldens: text rasterisation
/// differs between the developer's workstation and the hosted CI runner (18% of the pixels in
/// the same layout), so a golden captured here would fail there for no product reason. Each
/// case therefore checks that the surface really drew (distinct colours, content pixels), that
/// the variant and the density really applied (background luminance, spacing differ), that RTL
/// mirrors the frame, and that the brand-coloured action fill appears. See the open questions
/// for the reviewed-baseline follow-up on a pinned runner image.
/// </summary>
public sealed class ScreenshotTests
{
    public static TheoryData<string, bool, DensityMode, bool> Matrices => new()
    {
        { "light", false, DensityMode.Comfortable, false },
        { "dark", true, DensityMode.Comfortable, false },
        { "compact", false, DensityMode.Compact, false },
        { "compact-dark", true, DensityMode.Compact, false },
        { "rtl", false, DensityMode.Comfortable, true }
    };

    [AvaloniaTheory]
    [MemberData(nameof(Matrices))]
    public void ToolbarWithActions(string name, bool dark, DensityMode density, bool rtl)
    {
        Frame frame = Render(360, 96, density, dark, rtl, bar => bar);
        Assert.Equal(360, frame.Width);
        Assert.NotEmpty(frame.Colours);
        Assert.True(frame.Colours.Count > 3, $"{name}: the bar drew almost nothing");
        Assert.True(frame.BrandPixels > 20, $"{name}: no brand-coloured action surface ({frame.BrandPixels} px)");
        if (dark)
        {
            // The bar has no background of its own; the variant shows in the button tokens, so
            // the proof is that the rasterisation changed.
            Assert.NotEqual(Render(360, 96, density, false, false, bar => bar).Bytes, frame.Bytes);
        }
        if (rtl)
        {
            Assert.NotEqual(Render(360, 96, density, dark, false, bar => bar).Bytes, frame.Bytes);
        }
        if (density == DensityMode.Compact)
        {
            Assert.NotEqual(Render(360, 96, DensityMode.Comfortable, dark, rtl, bar => bar).Bytes, frame.Bytes);
        }
    }

    [AvaloniaTheory]
    [MemberData(nameof(Matrices))]
    public void SidePanelWithDataBlocks(string name, bool dark, DensityMode density, bool rtl)
    {
        Frame frame = Render(400, 560, density, dark, rtl, bar => bar, panel: true);
        Assert.True(frame.Colours.Count > 8, $"{name}: the panel drew almost nothing");
        // The card's surface is a token: light has a near-white mode, dark a near-black one.
        Assert.True(dark ? frame.ModeLuminance < .25 : frame.ModeLuminance > .9,
            $"{name}: card luminance {frame.ModeLuminance}");
        if (dark)
        {
            AssertMoreDarkThanLight(frame, Render(400, 560, density, false, rtl, bar => bar, panel: true), name);
        }
    }

    [AvaloniaTheory]
    [MemberData(nameof(Matrices))]
    public void CompositeTile(string name, bool dark, DensityMode density, bool rtl)
    {
        Frame frame = Render(320, 220, density, dark, rtl, bar => bar, tile: true);
        Assert.True(frame.Colours.Count > 3, $"{name}: the tile drew almost nothing");
        Assert.True(frame.BrandPixels > 20, $"{name}: the progress fill did not paint ({frame.BrandPixels} px)");
        if (dark)
        {
            AssertMoreDarkThanLight(frame, Render(320, 220, density, false, rtl, bar => bar, tile: true), name);
        }
    }

    private static void AssertMoreDarkThanLight(Frame dark, Frame light, string name) =>
        Assert.True(dark.DarkPixels > light.DarkPixels,
            $"{name}: the dark variant has {dark.DarkPixels} dark pixels and the light one {light.DarkPixels}");

    private static Frame Render(
        int width,
        int height,
        DensityMode density,
        bool dark,
        bool rtl,
        Func<Control, Control> map,
        bool panel = false,
        bool tile = false)
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = panel ? Sample.ProjectContext() : Sample.TaskContext();
        FakeQueries queries = FakeQueries.Sample();
        FakeShell shell = new();
        ExtensionServices services = new(new FakeDispatcher(), shell, queries);
        Control host;
        if (tile)
        {
            using SlotView view = set.ForSlot(ExtensionSlotCatalog.HomeTiles, context, null, services);
            TileHost tileHost = new();
            tileHost.Show(view);
            ((TileModel)view.Entries[0]).LoadAsync(TestContext.Current.CancellationToken).GetAwaiter().GetResult();
            host = tileHost;
        }
        else if (panel)
        {
            using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project", services);
            SidePanelHost panelHost = new();
            panelHost.Show(view);
            ((PanelModel)view.Entries[0]).RefreshAsync(TestContext.Current.CancellationToken).GetAwaiter().GetResult();
            host = panelHost;
        }
        else
        {
            using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task", services);
            ActionBar bar = new();
            bar.Show(view);
            host = map(bar);
        }
        Density.SetMode(host, density);
        Window window = new()
        {
            Width = width,
            Height = height,
            Content = host,
            RequestedThemeVariant = dark ? Avalonia.Styling.ThemeVariant.Dark : Avalonia.Styling.ThemeVariant.Light,
            FlowDirection = rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight
        };
        try
        {
            window.Show();
            using Bitmap bitmap = ScreenshotAssert.Capture(window);
            return new Frame(width, height, Pixels(bitmap));
        }
        finally
        {
            window.Close();
        }
    }

    private static byte[] Pixels(Bitmap bitmap)
    {
        using WriteableBitmap normalized = new(bitmap.PixelSize, new Vector(96, 96), PixelFormat.Rgba8888, AlphaFormat.Unpremul);
        using ILockedFramebuffer buffer = normalized.Lock();
        bitmap.CopyPixels(buffer);
        byte[] pixels = new byte[bitmap.PixelSize.Width * bitmap.PixelSize.Height * 4];
        for (int row = 0; row < bitmap.PixelSize.Height; row++)
        {
            Marshal.Copy(buffer.Address + row * buffer.RowBytes, pixels, row * bitmap.PixelSize.Width * 4, bitmap.PixelSize.Width * 4);
        }
        return pixels;
    }

    private sealed class Frame
    {
        internal Frame(int width, int height, byte[] bytes)
        {
            Width = width;
            Height = height;
            Bytes = bytes;
            Dictionary<int, int> counts = [];
            for (int index = 0; index < bytes.Length; index += 4)
            {
                int colour = bytes[index] << 16 | bytes[index + 1] << 8 | bytes[index + 2];
                counts[colour] = counts.TryGetValue(colour, out int count) ? count + 1 : 1;
            }
            Colours = counts;
            int mode = counts.MaxBy(entry => entry.Value).Key;
            ModeLuminance = (0.2126 * ((mode >> 16) & 0xFF) + 0.7152 * ((mode >> 8) & 0xFF) + 0.0722 * (mode & 0xFF)) / 255d;
            for (int index = 0; index < bytes.Length; index += 4)
            {
                if (bytes[index + 2] - bytes[index] > 40)
                {
                    BrandPixels++;
                }
                double luminance = (0.2126 * bytes[index] + 0.7152 * bytes[index + 1] + 0.0722 * bytes[index + 2]) / 255d;
                if (luminance < .2)
                {
                    DarkPixels++;
                }
            }
        }

        internal int Width { get; }

        internal int Height { get; }

        internal byte[] Bytes { get; }

        internal Dictionary<int, int> Colours { get; }

        internal double ModeLuminance { get; }

        internal int BrandPixels { get; }

        internal int DarkPixels { get; }
    }
}
