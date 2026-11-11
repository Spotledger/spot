using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Markup.Xaml.MarkupExtensions;
using Avalonia.Media;
using Avalonia.Styling;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// The matrix's theme-variant dimension (WP-UI-16.T3): the exact file stem every variant produces,
/// the refusal of a variant that contradicts the dark flag, that a null variant keeps the original
/// stem, that a high-contrast capture really paints differently from the light one, and that the
/// application's variant is restored even when the comparison fails.
/// </summary>
public sealed class ScreenshotVariantTests
{
    [Theory]
    [InlineData(ThemeVariantKind.Light, false, "light")]
    [InlineData(ThemeVariantKind.Dark, true, "dark")]
    [InlineData(ThemeVariantKind.HighContrast, false, "highcontrast")]
    [InlineData(ThemeVariantKind.HighContrastDark, true, "highcontrastdark")]
    public void TheStemSpellsTheVariantExactlyAsTheCommittedBaselinesDo(ThemeVariantKind variant, bool dark, string spelling)
    {
        var matrix = new ScreenshotMatrix("Baselines", 480, 140, DensityMode.Compact, dark: dark, rtl: true, Variant: variant);

        // The spelling theme tests committed their Urdu baselines under in WP-UI-15.T3.
        Assert.Equal($"urdu-field-text-480x140-compact-{spelling}-rtl", matrix.Stem("urdu-field-text"));
        Assert.Equal(variant, matrix.Palette);
        Assert.Equal(spelling, ScreenshotMatrix.PaletteName(variant));
    }

    [Theory]
    [InlineData(ThemeVariantKind.Light, true)]
    [InlineData(ThemeVariantKind.HighContrast, true)]
    [InlineData(ThemeVariantKind.Dark, false)]
    [InlineData(ThemeVariantKind.HighContrastDark, false)]
    public void ADarkFlagThatContradictsTheVariantIsRefused(ThemeVariantKind variant, bool dark)
    {
        var matrix = new ScreenshotMatrix("Baselines", 64, 48, dark: dark, Variant: variant);

        var error = Assert.Throws<ArgumentException>(() => matrix.Stem("matrix"));
        Assert.Contains("contradicts", error.Message);
    }

    [Theory]
    [InlineData(false, false, "light", "ltr")]
    [InlineData(true, false, "dark", "ltr")]
    [InlineData(false, true, "light", "rtl")]
    [InlineData(true, true, "dark", "rtl")]
    public void ANullVariantKeepsTodaysStemAndPalette(bool dark, bool rtl, string palette, string direction)
    {
        var matrix = new ScreenshotMatrix("Baselines", 64, 48, dark: dark, rtl: rtl);

        Assert.Null(matrix.Variant);
        // Exactly the stem `CommittedPrimitiveMatrix`'s eight committed baselines are named with.
        Assert.Equal($"matrix-64x48-comfortable-{palette}-{direction}", matrix.Stem("matrix"));
        Assert.Equal(dark ? ThemeVariantKind.Dark : ThemeVariantKind.Light, matrix.Palette);
    }

    [Fact]
    public void AnUnknownVariantIsRefused()
    {
        var matrix = new ScreenshotMatrix("Baselines", 64, 48, Variant: (ThemeVariantKind)42);

        Assert.Throws<ArgumentException>(() => matrix.Stem("matrix"));
    }

    [AvaloniaFact]
    public void AVariantThatContradictsTheDarkFlagIsRefusedBeforeRendering()
    {
        var application = Application.Current!;
        var previous = application.RequestedThemeVariant;
        try
        {
            var matrix = new ScreenshotMatrix("Baselines", 32, 24, dark: true, Variant: ThemeVariantKind.HighContrast);

            Assert.Throws<ArgumentException>(() => ScreenshotAssert.Matches("refused", new Border(), matrix));
            Assert.Equal(previous, application.RequestedThemeVariant);
        }
        finally
        {
            application.RequestedThemeVariant = previous;
        }
    }

    /// <summary>
    /// The variant has to reach the pixels: a tiny control painted from a token is the light
    /// palette's colour in one capture and the high-contrast palette's in the other, and no
    /// tolerance may hide that (channel delta 0, zero changed-pixel fraction).
    /// </summary>
    [AvaloniaFact]
    public void AHighContrastCaptureDiffersFromTheLightCaptureOfTheSameControl()
    {
        using var light = new TemporaryDirectory();
        using var contrast = new TemporaryDirectory();
        var previousUpdate = Environment.GetEnvironmentVariable("SPOT_UPDATE_SHOTS");
        var previousAlias = Environment.GetEnvironmentVariable("UPDATE_BASELINES");
        try
        {
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", null);
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", "1");
            ScreenshotAssert.Matches("token", TokenCanvas(),
                new ScreenshotMatrix(light.Path, 24, 16, Variant: ThemeVariantKind.Light));
            ScreenshotAssert.Matches("token", TokenCanvas(),
                new ScreenshotMatrix(contrast.Path, 24, 16, Variant: ThemeVariantKind.HighContrast));
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", null);

            var lightPixels = Pixels(Path.Combine(light.Path, "token-24x16-comfortable-light-ltr.png"));
            var contrastPixels = Pixels(Path.Combine(contrast.Path, "token-24x16-comfortable-highcontrast-ltr.png"));
            Assert.Equal(lightPixels.Length, contrastPixels.Length);

            var comparison = ScreenshotAssert.Compare(lightPixels, contrastPixels, new ScreenshotTolerance(0, 0));
            Assert.False(comparison.Matches, "The high-contrast capture was pixel-identical to the light one.");
            Assert.True(comparison.ChangedPixels > 0);
        }
        finally
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", previousUpdate);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", previousAlias);
        }
    }

    /// <summary>
    /// A variant capture installs the palette on the application for the capture (a window-only
    /// variant paints a dark screen with a light grid). The failure path must put it back too,
    /// otherwise every later case in the process renders in the wrong palette.
    /// </summary>
    [AvaloniaFact]
    public void TheApplicationVariantIsRestoredByAFailingComparison()
    {
        using var baseline = new TemporaryDirectory();
        using var output = new TemporaryDirectory();
        var application = Application.Current!;
        var previous = application.RequestedThemeVariant;
        try
        {
            application.RequestedThemeVariant = ThemeVariant.Light;
            var matrix = new ScreenshotMatrix(baseline.Path, 32, 24, dark: true, Variant: ThemeVariantKind.HighContrastDark)
            {
                OutputDirectory = output.Path,
            };

            var error = Assert.Throws<ScreenshotMismatchException>(() => ScreenshotAssert.Matches("restored",
                new Border { Background = Brushes.Red }, matrix));

            Assert.Contains("highcontrastdark", error.Message);
            Assert.Contains("Missing", error.Message);
            Assert.Equal(ThemeVariant.Light, application.RequestedThemeVariant);
        }
        finally
        {
            application.RequestedThemeVariant = previous;
        }
    }

    /// <summary>A tiny control whose background is a token, so the palette decides its colour.</summary>
    private static Border TokenCanvas() => new()
    {
        Width = 24,
        Height = 16,
        [!Border.BackgroundProperty] = new DynamicResourceExtension(Tokens.TokenColorSurfaceRaised),
    };

    private static byte[] Pixels(string path)
    {
        using var bitmap = new Avalonia.Media.Imaging.Bitmap(path);
        using var normalized = new Avalonia.Media.Imaging.WriteableBitmap(bitmap.PixelSize, new Vector(96, 96),
            Avalonia.Platform.PixelFormat.Rgba8888, Avalonia.Platform.AlphaFormat.Unpremul);
        using var buffer = normalized.Lock();
        bitmap.CopyPixels(buffer);
        var pixels = new byte[checked(bitmap.PixelSize.Width * bitmap.PixelSize.Height * 4)];
        for (var row = 0; row < bitmap.PixelSize.Height; row++)
        {
            System.Runtime.InteropServices.Marshal.Copy(
                buffer.Address + row * buffer.RowBytes, pixels, row * bitmap.PixelSize.Width * 4, bitmap.PixelSize.Width * 4);
        }

        return pixels;
    }
}
