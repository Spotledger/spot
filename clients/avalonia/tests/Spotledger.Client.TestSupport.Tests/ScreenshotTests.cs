using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

public sealed class ScreenshotTests
{
    [AvaloniaTheory]
    [InlineData(false, false, false)]
    [InlineData(false, false, true)]
    [InlineData(false, true, false)]
    [InlineData(false, true, true)]
    [InlineData(true, false, false)]
    [InlineData(true, false, true)]
    [InlineData(true, true, false)]
    [InlineData(true, true, true)]
    public void CommittedPrimitiveMatrix(bool compact, bool dark, bool rtl)
    {
        // Text-free raster primitives are intentionally identical across platforms, but every
        // baseline still lives under Baselines/<platform>/ (WP-UI-16.T2): a platform never reads
        // another platform's file, and a missing one fails loudly with the update command.
        var density = compact ? DensityMode.Compact : DensityMode.Comfortable;
        var panel = new StackPanel { Orientation = Avalonia.Layout.Orientation.Horizontal, Spacing = compact ? 2 : 6 };
        panel.Children.Add(new Border { Width = 12, Height = 16, Background = Brushes.Orange });
        panel.Children.Add(new Border { Width = 20, Height = 8, Background = Brushes.CornflowerBlue });
        var control = new Border { Padding = new Avalonia.Thickness(compact ? 2 : 4), Background = dark ? Brushes.Black : Brushes.White, Child = panel };
        var directory = ScreenshotAssert.PlatformBaselineDirectory(
            Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "Baselines")));
        ScreenshotAssert.Matches("matrix", control, new(directory, 64, 48, density, dark, rtl));
        Assert.Equal(density, Density.GetMode(panel));
        Assert.Equal(rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight, panel.FlowDirection);
        // Avalonia applies RTL mirroring during rendering, not Bounds/TranslatePoint.
        // Matches already proves actual pixels equal this golden; inspect its orange centroid.
        var filename = $"matrix-64x48-{density.ToString().ToLowerInvariant()}-{(dark ? "dark" : "light")}-{(rtl ? "rtl" : "ltr")}.png";
        using var golden = new Avalonia.Media.Imaging.Bitmap(Path.Combine(directory, filename));
        using var pixels = new Avalonia.Media.Imaging.WriteableBitmap(golden.PixelSize, new Vector(96, 96),
            Avalonia.Platform.PixelFormat.Rgba8888, Avalonia.Platform.AlphaFormat.Unpremul);
        using var buffer = pixels.Lock();
        golden.CopyPixels(buffer);
        var orangeX = rtl ? 64 - (compact ? 8 : 10) : compact ? 8 : 10;
        var offset = 24 * buffer.RowBytes + orangeX * 4;
        Assert.Equal(255, System.Runtime.InteropServices.Marshal.ReadByte(buffer.Address, offset));
        Assert.Equal(165, System.Runtime.InteropServices.Marshal.ReadByte(buffer.Address, offset + 1));
        Assert.Equal(0, System.Runtime.InteropServices.Marshal.ReadByte(buffer.Address, offset + 2));
    }

    [Fact]
    public void PixelToleranceHasExplicitBoundaries()
    {
        byte[] expected = [0, 0, 0, 255, 10, 20, 30, 255];
        byte[] actual = [2, 0, 0, 255, 14, 20, 30, 255];
        Assert.True(ScreenshotAssert.Compare(expected, actual, new(4, 0)).Matches);
        Assert.True(ScreenshotAssert.Compare(expected, actual, new(2, .5)).Matches);
        Assert.False(ScreenshotAssert.Compare(expected, actual, new(2, .49)).Matches);
        Assert.False(ScreenshotAssert.Compare(expected, actual, new(0, 0)).Matches);
        Assert.Throws<ArgumentOutOfRangeException>(() => new ScreenshotTolerance(256, 0));
        Assert.Throws<ArgumentOutOfRangeException>(() => new ScreenshotTolerance(0, double.NaN));
    }

    [AvaloniaFact]
    public void DimensionMismatchFailsEvenWithMaximumPixelTolerance()
    {
        using var baseline = new TemporaryDirectory();
        using var output = new TemporaryDirectory();
        var path = Path.Combine(baseline.Path, "dimensions-64x48-comfortable-light-ltr.png");
        using (var bitmap = new Avalonia.Media.Imaging.WriteableBitmap(new Avalonia.PixelSize(1, 1), new Avalonia.Vector(96, 96),
            Avalonia.Platform.PixelFormat.Rgba8888, Avalonia.Platform.AlphaFormat.Unpremul))
            bitmap.Save(path, new Avalonia.Media.Imaging.PngBitmapEncoderOptions());
        var previous = Environment.GetEnvironmentVariable("SPOT_UPDATE_SHOTS");
        var previousAlias = Environment.GetEnvironmentVariable("UPDATE_BASELINES");
        try
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", "0");
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", "0");
            Assert.Throws<ScreenshotMismatchException>(() => ScreenshotAssert.Matches("dimensions", new Border { Background = Brushes.Red },
                new(baseline.Path, 64, 48) { OutputDirectory = output.Path, Tolerance = new(255, 1) }));
            using var diff = new Avalonia.Media.Imaging.Bitmap(Assert.Single(Directory.GetFiles(output.Path, "*-diff.png")));
            Assert.Equal(new Avalonia.PixelSize(64, 48), diff.PixelSize);
            using var expected = new Avalonia.Media.Imaging.Bitmap(Assert.Single(Directory.GetFiles(output.Path, "*-expected.png")));
            Assert.Equal(new Avalonia.PixelSize(1, 1), expected.PixelSize);
        }
        finally
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", previous);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", previousAlias);
        }
    }

    [AvaloniaFact]
    public void UpdateIsExplicitAndMismatchProducesRealReviewArtifacts()
    {
        using var baseline = new TemporaryDirectory();
        using var output = new TemporaryDirectory();
        var matrix = new ScreenshotMatrix(baseline.Path, 64, 48, DensityMode.Compact, dark: true, rtl: true)
        { OutputDirectory = output.Path, Tolerance = new(0, 0) };
        var previous = Environment.GetEnvironmentVariable("SPOT_UPDATE_SHOTS");
        var previousAlias = Environment.GetEnvironmentVariable("UPDATE_BASELINES");
        var control = new Border { Background = Brushes.Red };
        try
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", null);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", null);
            Assert.Throws<ScreenshotMismatchException>(() => ScreenshotAssert.Matches("solid", control, matrix));
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", "1");
            ScreenshotAssert.Matches("solid", control, matrix);
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", null);
            ScreenshotAssert.Matches("solid", control, matrix);
            Assert.Equal(DensityMode.Compact, Density.GetMode(control));
            Assert.Equal(FlowDirection.RightToLeft, control.FlowDirection);
            control.Background = Brushes.Blue;
            Assert.Throws<ScreenshotMismatchException>(() => ScreenshotAssert.Matches("solid", control, matrix));
            Assert.Single(Directory.GetFiles(output.Path, "solid-64x48-compact-dark-rtl-actual.png"));
            Assert.Single(Directory.GetFiles(output.Path, "*-missing-actual.png"));
            Assert.Single(Directory.GetFiles(output.Path, "*-expected.png", SearchOption.AllDirectories));
            Assert.Single(Directory.GetFiles(output.Path, "*-diff.png", SearchOption.AllDirectories));
            Assert.Contains("expected", File.ReadAllText(Assert.Single(Directory.GetFiles(output.Path, "*.html", SearchOption.AllDirectories))));
            Assert.Throws<ArgumentException>(() => ScreenshotAssert.Matches("../escape", control, matrix));
        }
        finally
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", previous);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", previousAlias);
        }
    }

    [AvaloniaFact]
    public void MissingBaselineFailsWithTheExactPerPlatformUpdateCommand()
    {
        using var baseline = new TemporaryDirectory();
        using var output = new TemporaryDirectory();
        var previous = Environment.GetEnvironmentVariable("SPOT_UPDATE_SHOTS");
        var previousAlias = Environment.GetEnvironmentVariable("UPDATE_BASELINES");
        try
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", "0");
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", "0");
            var directory = ScreenshotAssert.PlatformBaselineDirectory(baseline.Path);
            var error = Assert.Throws<ScreenshotMismatchException>(() => ScreenshotAssert.Matches("absent",
                new Border { Background = Brushes.Red }, new(directory, 32, 24) { OutputDirectory = output.Path }));
            Assert.Contains("UPDATE_BASELINES=1 dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests", error.Message);
            Assert.Contains(ScreenshotAssert.PlatformName(), error.Message);
            Assert.Contains(directory, error.Message);
            // The real capture is kept for review; no expected image or diff is fabricated.
            Assert.Single(Directory.GetFiles(output.Path, "absent-32x24-comfortable-light-ltr-missing-actual.png"));
            Assert.Empty(Directory.GetFiles(output.Path, "*-expected.png", SearchOption.AllDirectories));
            Assert.Equal(Path.Combine("Baselines", ScreenshotAssert.PlatformName()), ScreenshotAssert.PlatformBaselineDirectory("Baselines"));
            Assert.False(ScreenshotAssert.UpdateRequested);
        }
        finally
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", previous);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", previousAlias);
        }
    }

    [Fact]
    public void TheUpdateSwitchIsExplicitlyHonouredFromEitherName()
    {
        var previous = Environment.GetEnvironmentVariable("SPOT_UPDATE_SHOTS");
        var previousAlias = Environment.GetEnvironmentVariable("UPDATE_BASELINES");
        try
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", null);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", null);
            Assert.False(ScreenshotAssert.UpdateRequested);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", "1");
            Assert.True(ScreenshotAssert.UpdateRequested);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", "0");
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", "1");
            Assert.True(ScreenshotAssert.UpdateRequested);
            Assert.Contains("--filter FullyQualifiedName~CommittedPrimitiveMatrix", ScreenshotAssert.UpdateCommand("CommittedPrimitiveMatrix"));
            Assert.DoesNotContain("--filter", ScreenshotAssert.UpdateCommand());
        }
        finally
        {
            Environment.SetEnvironmentVariable("SPOT_UPDATE_SHOTS", previous);
            Environment.SetEnvironmentVariable("UPDATE_BASELINES", previousAlias);
        }
    }
}
