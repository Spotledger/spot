using Avalonia;
using Avalonia.Controls;
using Avalonia.Media.Imaging;
using Avalonia.Platform;
using System.Runtime.InteropServices;

namespace Spotledger.Client.TestSupport.A11y;

/// <summary>
/// An RGBA8 frame of what the renderer actually drew. WCAG luminance and contrast are computed
/// from these bytes, never from a token table: the audit judges the pixels a person sees.
/// </summary>
internal sealed class PixelBuffer(int width, int height, byte[] pixels)
{
    public int Width { get; } = width;
    public int Height { get; } = height;
    public byte[] Pixels { get; } = pixels;

    public readonly record struct Rgba8(byte R, byte G, byte B, byte A)
    {
        public int Key => R << 24 | G << 16 | B << 8 | A;
    }

    /// <summary>Capture the window's current frame, pumping the render loop like ScreenshotAssert.</summary>
    public static PixelBuffer Of(Window window)
    {
        ArgumentNullException.ThrowIfNull(window);
        using var bitmap = ScreenshotAssert.Capture(window);
        return From(bitmap);
    }

    public static PixelBuffer From(Bitmap bitmap)
    {
        ArgumentNullException.ThrowIfNull(bitmap);
        using var normalized = new WriteableBitmap(bitmap.PixelSize, new Vector(96, 96), PixelFormat.Rgba8888, AlphaFormat.Unpremul);
        using var buffer = normalized.Lock();
        bitmap.CopyPixels(buffer);
        var pixels = new byte[checked(bitmap.PixelSize.Width * bitmap.PixelSize.Height * 4)];
        for (var row = 0; row < bitmap.PixelSize.Height; row++)
            Marshal.Copy(buffer.Address + row * buffer.RowBytes, pixels, row * bitmap.PixelSize.Width * 4, bitmap.PixelSize.Width * 4);
        return new PixelBuffer(bitmap.PixelSize.Width, bitmap.PixelSize.Height, pixels);
    }

    public bool Contains(int x, int y) => x >= 0 && y >= 0 && x < Width && y < Height;

    public Rgba8 At(int x, int y)
    {
        var offset = (y * Width + x) * 4;
        return new(Pixels[offset], Pixels[offset + 1], Pixels[offset + 2], Pixels[offset + 3]);
    }

    /// <summary>WCAG 2.x relative luminance of an sRGB colour (alpha is composited on white).</summary>
    public static double RelativeLuminance(Rgba8 color)
    {
        static double Channel(byte value, byte alpha)
        {
            var composited = alpha == 255 ? value : (value * alpha + 255 * (255 - alpha)) / 255.0;
            var srgb = composited / 255.0;
            return srgb <= 0.03928 ? srgb / 12.92 : Math.Pow((srgb + 0.055) / 1.055, 2.4);
        }

        return 0.2126 * Channel(color.R, color.A) + 0.7152 * Channel(color.G, color.A) + 0.0722 * Channel(color.B, color.A);
    }

    /// <summary>WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white).</summary>
    public static double ContrastRatio(Rgba8 first, Rgba8 second)
    {
        var one = RelativeLuminance(first);
        var two = RelativeLuminance(second);
        var lighter = Math.Max(one, two);
        var darker = Math.Min(one, two);
        return (lighter + 0.05) / (darker + 0.05);
    }

    /// <summary>Composite a colour on the canvas colour beneath it (alpha 0 counts as the given base).</summary>
    public static Rgba8 Composite(Rgba8 top, Rgba8 bottom)
    {
        if (top.A == 255) return top;
        var alpha = top.A / 255.0;
        byte Mix(byte above, byte below) => (byte)Math.Round(above * alpha + below * (1 - alpha));
        return new(Mix(top.R, bottom.R), Mix(top.G, bottom.G), Mix(top.B, bottom.B), 255);
    }

    public Rect Bounds => new(0, 0, Width, Height);

    /// <summary>Clip a rectangle to the frame; an empty rectangle means there is nothing to sample.</summary>
    public PixelRect Clip(Rect rect)
    {
        var left = Math.Clamp((int)Math.Floor(rect.X), 0, Width);
        var top = Math.Clamp((int)Math.Floor(rect.Y), 0, Height);
        var right = Math.Clamp((int)Math.Ceiling(rect.Right), 0, Width);
        var bottom = Math.Clamp((int)Math.Ceiling(rect.Bottom), 0, Height);
        return new PixelRect(left, top, Math.Max(0, right - left), Math.Max(0, bottom - top));
    }
}

/// <summary>A small integer rectangle: no floating-point coordinates cross a rule boundary.</summary>
internal readonly record struct PixelRect(int X, int Y, int Width, int Height)
{
    public bool IsEmpty => Width <= 0 || Height <= 0;

    public bool Contains(int x, int y) => x >= X && y >= Y && x < X + Width && y < Y + Height;
}
