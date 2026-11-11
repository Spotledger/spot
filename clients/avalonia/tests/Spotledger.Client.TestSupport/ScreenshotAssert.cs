using System.Globalization;
using System.Net;
using System.Runtime.InteropServices;
using System.Text.RegularExpressions;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform;
using Avalonia.Styling;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;

namespace Spotledger.Client.TestSupport;

public sealed record ScreenshotTolerance
{
    public int ChannelDelta { get; }
    public double ChangedPixelFraction { get; }
    public ScreenshotTolerance(int channelDelta = 2, double changedPixelFraction = .001)
    {
        if (channelDelta is < 0 or > 255) throw new ArgumentOutOfRangeException(nameof(channelDelta));
        if (!double.IsFinite(changedPixelFraction) || changedPixelFraction is < 0 or > 1)
            throw new ArgumentOutOfRangeException(nameof(changedPixelFraction));
        (ChannelDelta, ChangedPixelFraction) = (channelDelta, changedPixelFraction);
    }
}

/// <summary>
/// One screenshot case: a fixed logical size, the density, the direction and the palette.
///
/// <para><c>dark</c> is the case's original light/dark switch and stays first-class.
/// <see cref="Variant"/> adds the two high-contrast palettes: when it is set, the variant decides
/// the palette and the file stem and must agree with <c>dark</c>; when it is null the case behaves
/// exactly as before, so the committed light/dark baselines keep their names and their pixels.</para>
/// </summary>
public sealed record ScreenshotMatrix(string BaselineDirectory, int Width, int Height, DensityMode Density = DensityMode.Comfortable,
    bool dark = false, bool rtl = false, ThemeVariantKind? Variant = null)
{
    public ScreenshotTolerance Tolerance { get; init; } = new();
    public string? OutputDirectory { get; init; }
    // Only use after documenting a real rendering difference in testing.md.
    public bool PerOs { get; init; }

    /// <summary>
    /// The palette this case renders in: the explicit variant, else <c>dark</c>. A variant that
    /// disagrees with <c>dark</c> about a dark background is refused rather than silently winning,
    /// so a file name and a pipeline can never describe two different pictures.
    /// </summary>
    public ThemeVariantKind Palette
    {
        get
        {
            if (Variant is not { } variant) return dark ? ThemeVariantKind.Dark : ThemeVariantKind.Light;
            if (!Enum.IsDefined(variant)) throw new ArgumentException("Unknown theme variant.", nameof(Variant));
            if (variant.IsDark() != dark)
            {
                throw new ArgumentException(
                    $"dark: {dark.ToString().ToLowerInvariant()} contradicts Variant: {variant}; a dark palette needs dark: true.",
                    nameof(Variant));
            }
            return variant;
        }
    }

    /// <summary>
    /// The baseline file stem, <c>name-WxH-density-palette-direction</c> with the palette spelled
    /// <c>light</c>, <c>dark</c>, <c>highcontrast</c> or <c>highcontrastdark</c>, plus the optional
    /// per-OS suffix. The high-contrast spellings are the ones WP-UI-15.T3 committed its Urdu
    /// baselines under, so every case keeps its exact file name.
    /// </summary>
    public string Stem(string name)
    {
        ScreenshotAssert.ValidateCaseName(name);
        var stem = FormattableString.Invariant(
            $"{name}-{Width}x{Height}-{Density.ToString().ToLowerInvariant()}-{PaletteName(Palette)}-{(rtl ? "rtl" : "ltr")}");
        return PerOs ? stem + "-" + ScreenshotAssert.PlatformName() : stem;
    }

    /// <summary>The file stem's spelling of a palette: what every committed baseline is called.</summary>
    public static string PaletteName(ThemeVariantKind palette) => palette switch
    {
        ThemeVariantKind.Light => "light",
        ThemeVariantKind.Dark => "dark",
        ThemeVariantKind.HighContrast => "highcontrast",
        ThemeVariantKind.HighContrastDark => "highcontrastdark",
        _ => throw new ArgumentOutOfRangeException(nameof(palette), palette, "Unknown theme variant."),
    };
}

public sealed class ScreenshotMismatchException(string message) : Exception(message);
public sealed record PixelComparison(bool Matches, int ChangedPixels, byte[] Diff);

public static partial class ScreenshotAssert
{
    private static readonly string[] ReviewKinds = ["expected", "actual", "diff"];

    /// <summary>The platform directory a baseline lives in: font rasterization differs per OS.</summary>
    public static string PlatformName() =>
        OperatingSystem.IsWindows() ? "windows"
        : OperatingSystem.IsLinux() ? "linux"
        : OperatingSystem.IsMacOS() ? "macos"
        : throw new PlatformNotSupportedException("Screenshot baselines are committed for Windows, Linux and macOS only.");

    /// <summary>
    /// The per-platform baseline directory below <paramref name="root"/> (WP-UI-16.T2). A case has
    /// one reviewed baseline per platform; there is never a fallback to another platform's file.
    /// </summary>
    public static string PlatformBaselineDirectory(string root)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(root);
        return Path.Combine(root, PlatformName());
    }

    /// <summary>
    /// The explicit baseline-update switch. <c>UPDATE_BASELINES=1</c> is the documented spelling;
    /// <c>SPOT_UPDATE_SHOTS=1</c> (part 1, and what CI pins to <c>0</c>) still works.
    /// </summary>
    public static bool UpdateRequested =>
        Environment.GetEnvironmentVariable("UPDATE_BASELINES") == "1"
        || Environment.GetEnvironmentVariable("SPOT_UPDATE_SHOTS") == "1";

    /// <summary>The command that creates a missing baseline, printed on failure. Never fall back silently.</summary>
    public static string UpdateCommand(string? testFilter = null) =>
        "UPDATE_BASELINES=1 dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests"
        + (string.IsNullOrWhiteSpace(testFilter) ? string.Empty : $" --filter FullyQualifiedName~{testFilter}");
    public static PixelComparison Compare(byte[] expected, byte[] actual, ScreenshotTolerance tolerance)
    {
        ArgumentNullException.ThrowIfNull(expected);
        ArgumentNullException.ThrowIfNull(actual);
        ArgumentNullException.ThrowIfNull(tolerance);
        if (expected.Length != actual.Length || actual.Length == 0 || actual.Length % 4 != 0)
            throw new ArgumentException("Expected equal, nonempty RGBA buffers.", nameof(actual));
        var changed = 0;
        var diff = new byte[actual.Length];
        for (var pixel = 0; pixel < actual.Length; pixel += 4)
        {
            var mismatch = false;
            for (var channel = 0; channel < 4; channel++)
                mismatch |= Math.Abs(actual[pixel + channel] - expected[pixel + channel]) > tolerance.ChannelDelta;
            if (mismatch) changed++;
            diff[pixel] = mismatch ? (byte)255 : (byte)0;
            diff[pixel + 3] = 255;
        }
        return new((double)changed / (actual.Length / 4) <= tolerance.ChangedPixelFraction, changed, diff);
    }

    /// <summary>Rejects a case name that is not a plain identifier (never a path).</summary>
    internal static void ValidateCaseName(string name)
    {
        if (name is null || !SafeName().IsMatch(name)) throw new ArgumentException("Use a plain screenshot case name.", nameof(name));
    }

    /// <summary>
    /// Renders <paramref name="control"/> in the matrix's fixed-size undecorated host, compares it
    /// with the committed baseline and closes the host afterwards. A variant case also applies the
    /// palette to the application for the duration of the capture and restores the previous one
    /// (the grid and other token brushes resolve against <c>Application.ActualThemeVariant</c>, so a
    /// window-only variant would paint a dark screen with a light grid). The explicit update switch
    /// is the only way a baseline is written.
    /// </summary>
    public static void Matches(string name, Control control, ScreenshotMatrix matrix)
    {
        var window = MatchesWindow(name, control, matrix);
        Close(window);
    }

    /// <summary>
    /// <see cref="Matches"/> that leaves the host open so a case can assert the window it asked for
    /// (direction, density, variant) before closing it with <see cref="Close"/>. The application's
    /// theme variant is already restored, and the host is closed, when this throws.
    /// </summary>
    public static Window MatchesWindow(string name, Control control, ScreenshotMatrix matrix)
    {
        ArgumentNullException.ThrowIfNull(control);
        ArgumentNullException.ThrowIfNull(matrix);
        ValidateCaseName(name);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(matrix.Width);
        ArgumentOutOfRangeException.ThrowIfNegativeOrZero(matrix.Height);
        if (!Enum.IsDefined(matrix.Density)) throw new ArgumentException("Unknown density.", nameof(matrix));
        var stem = matrix.Stem(name);
        var palette = matrix.Palette;
        Dispatcher.UIThread.VerifyAccess();
        var baseline = Path.Combine(matrix.BaselineDirectory, stem + ".png");
        var application = Application.Current;
        var previousApplicationVariant = application?.RequestedThemeVariant;
        if (matrix.Variant is not null && application is not null)
            application.RequestedThemeVariant = palette.ToThemeVariant();
        var window = new Window
        {
            Width = matrix.Width, Height = matrix.Height, CanResize = false,
            WindowDecorations = WindowDecorations.None,
            RequestedThemeVariant = palette.ToThemeVariant(),
            FlowDirection = matrix.rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight,
            Content = control,
        };
        // A variant case paints the variant's own canvas: a host that installs no built-in theme
        // has no window brush of its own, so the frame would not be the palette's. The default
        // (no variant) case keeps the host's window, exactly as its committed baselines did.
        if (matrix.Variant is not null && application is not null
            && SpotledgerTheme.TryGetToken(application, Tokens.TokenColorSurfaceCanvas, palette, out var canvas)
            && canvas is Color canvasColor)
        {
            window.Background = new SolidColorBrush(canvasColor);
        }
        Density.SetMode(control, matrix.Density);
        control.FlowDirection = window.FlowDirection;
        try
        {
            window.Show();
            using var actual = matrix.Variant is null ? Capture(window) : CaptureSettled(window);
            if (actual.PixelSize != new PixelSize(matrix.Width, matrix.Height))
                throw new ScreenshotMismatchException($"Capture dimensions {actual.PixelSize} differ from fixed matrix size.");
            if (UpdateRequested)
            {
                Directory.CreateDirectory(matrix.BaselineDirectory);
                actual.Save(baseline, new PngBitmapEncoderOptions());
                return window;
            }
            var output = matrix.OutputDirectory ?? Environment.GetEnvironmentVariable("SPOT_SHOTS_OUT") ?? Path.Combine(AppContext.BaseDirectory, "screenshot-diffs");
            if (!File.Exists(baseline))
            {
                Directory.CreateDirectory(output);
                actual.Save(Path.Combine(output, stem + "-missing-actual.png"), new PngBitmapEncoderOptions());
                throw new ScreenshotMismatchException(
                    $"Missing {PlatformName()} baseline {baseline}. Create it explicitly, then review every image before committing: "
                    + $"{UpdateCommand()} (never copy another platform's baseline; UPDATE_BASELINES=0 is the comparison mode, which CI pins).");
            }
            using var expected = new Bitmap(baseline);
            var dimensionsMatch = expected.PixelSize == actual.PixelSize;
            var comparison = dimensionsMatch ? Compare(Pixels(expected), Pixels(actual), matrix.Tolerance) : null;
            if (comparison?.Matches == true) return window;
            Directory.CreateDirectory(output);
            actual.Save(Path.Combine(output, stem + "-actual.png"), new PngBitmapEncoderOptions());
            expected.Save(Path.Combine(output, stem + "-expected.png"), new PngBitmapEncoderOptions());
            // Dimension diffs use a max-size canvas: missing pixels are magenta, changed pixels red.
            var diffSize = new PixelSize(Math.Max(expected.PixelSize.Width, actual.PixelSize.Width), Math.Max(expected.PixelSize.Height, actual.PixelSize.Height));
            var diff = comparison?.Diff ?? DimensionDiff(expected, actual, diffSize, matrix.Tolerance);
            SavePixels(Path.Combine(output, stem + "-diff.png"), diff, diffSize);
            var title = WebUtility.HtmlEncode(stem);
            var images = string.Join("\n", ReviewKinds.Select(kind =>
                $"<figure><figcaption>{kind}</figcaption><img alt=\"{kind}\" src=\"{title}-{kind}.png\"></figure>"));
            File.WriteAllText(Path.Combine(output, stem + ".html"),
                $"<!doctype html><meta charset=\"utf-8\"><title>{title}</title><h1>{title}</h1><p>Channel delta {matrix.Tolerance.ChannelDelta}; changed pixel fraction {matrix.Tolerance.ChangedPixelFraction.ToString(CultureInfo.InvariantCulture)}. Dimensions match: {dimensionsMatch}.</p>{images}");
            throw new ScreenshotMismatchException($"Screenshot {stem} mismatch ({comparison?.ChangedPixels.ToString(CultureInfo.InvariantCulture) ?? "dimensions"}); review {output}.");
        }
        catch
        {
            window.Content = null;
            window.Close();
            throw;
        }
        finally
        {
            if (matrix.Variant is not null && application is not null)
                application.RequestedThemeVariant = previousApplicationVariant;
        }
    }

    /// <summary>Closes a host a case opened with <see cref="MatchesWindow"/>.</summary>
    public static void Close(Window window)
    {
        ArgumentNullException.ThrowIfNull(window);
        window.Content = null;
        window.Close();
        Dispatcher.UIThread.RunJobs();
    }

    public static Bitmap Capture(Window window)
    {
        ArgumentNullException.ThrowIfNull(window);
        for (var attempt = 0; attempt < 20; attempt++)
        {
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            var frame = window.CaptureRenderedFrame();
            if (frame is not null) return frame;
        }
        throw new ScreenshotMismatchException("No rendered frame after 20 pumps; initialize Skia and disable headless drawing.");
    }

    /// <summary>
    /// Captures until two consecutive frames are byte-identical. The first frame after a window is
    /// shown can still be a partially arranged one (text measured before the glyph cache is warm),
    /// which would make a baseline depend on test order; settling removes that whole class of flake
    /// and fails loudly (bounded, never unbounded) if a frame never settles. Variant cases use it
    /// because their committed baselines were recorded from a settled frame.
    /// </summary>
    private static Bitmap CaptureSettled(Window window)
    {
        Bitmap? previous = null;
        byte[]? previousPixels = null;
        try
        {
            for (var attempt = 0; attempt < 20; attempt++)
            {
                for (var round = 0; round < 3; round++)
                {
                    window.UpdateLayout();
                    Dispatcher.UIThread.RunJobs();
                    AvaloniaHeadlessPlatform.ForceRenderTimerTick();
                    Dispatcher.UIThread.RunJobs();
                }
                var frame = Capture(window);
                var pixels = Pixels(frame);
                if (previous is not null && previousPixels!.AsSpan().SequenceEqual(pixels))
                {
                    frame.Dispose();
                    return previous;
                }
                previous?.Dispose();
                previous = frame;
                previousPixels = pixels;
            }
        }
        catch
        {
            previous?.Dispose();
            throw;
        }
        previous?.Dispose();
        throw new ScreenshotMismatchException("The rendered frame never settled after 20 pumps; refusing to compare an unstable image.");
    }

    private static byte[] Pixels(Bitmap bitmap)
    {
        using var normalized = new WriteableBitmap(bitmap.PixelSize, new Vector(96, 96), PixelFormat.Rgba8888, AlphaFormat.Unpremul);
        using var buffer = normalized.Lock();
        bitmap.CopyPixels(buffer);
        var pixels = new byte[checked(bitmap.PixelSize.Width * bitmap.PixelSize.Height * 4)];
        for (var row = 0; row < bitmap.PixelSize.Height; row++)
            Marshal.Copy(buffer.Address + row * buffer.RowBytes, pixels, row * bitmap.PixelSize.Width * 4, bitmap.PixelSize.Width * 4);
        return pixels;
    }

    private static void SavePixels(string path, byte[] pixels, PixelSize size)
    {
        using var bitmap = new WriteableBitmap(size, new Vector(96, 96), PixelFormat.Rgba8888, AlphaFormat.Unpremul);
        using (var buffer = bitmap.Lock())
            for (var row = 0; row < size.Height; row++)
                Marshal.Copy(pixels, row * size.Width * 4, buffer.Address + row * buffer.RowBytes, size.Width * 4);
        bitmap.Save(path, new PngBitmapEncoderOptions());
    }

    private static byte[] DimensionDiff(Bitmap expected, Bitmap actual, PixelSize size, ScreenshotTolerance tolerance)
    {
        var left = Pixels(expected);
        var right = Pixels(actual);
        var result = new byte[checked(size.Width * size.Height * 4)];
        for (var y = 0; y < size.Height; y++)
            for (var x = 0; x < size.Width; x++)
            {
                var offset = (y * size.Width + x) * 4;
                var absent = x >= expected.PixelSize.Width || y >= expected.PixelSize.Height || x >= actual.PixelSize.Width || y >= actual.PixelSize.Height;
                var changed = absent;
                if (!absent)
                    for (var c = 0; c < 4; c++)
                        changed |= Math.Abs(left[(y * expected.PixelSize.Width + x) * 4 + c] - right[(y * actual.PixelSize.Width + x) * 4 + c]) > tolerance.ChannelDelta;
                result[offset] = changed ? (byte)255 : (byte)0;
                result[offset + 2] = absent ? (byte)255 : (byte)0;
                result[offset + 3] = 255;
            }
        return result;
    }

    [GeneratedRegex(@"^[A-Za-z0-9][A-Za-z0-9_-]*$")]
    private static partial Regex SafeName();
}
