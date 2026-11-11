using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Api;
using Spotledger.Client.Controls;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

/// <summary>The screenshot set: comfortable and compact density, light and dark, left to right and right to
/// left, and the empty, error, loading and truncated states. Captures land in this test project's own output
/// folder (<c>screenshots/reports</c>), and each is checked to hold a real picture, not an empty frame.</summary>
public sealed class ScreenshotTests
{
    private static readonly string[] Compared = ["cmp-normal.png", "cmp-empty.png", "cmp-error.png", "cmp-truncated.png", "cmp-loading.png"];

    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "reports");
        Directory.CreateDirectory(dir);
        return dir;
    }

    private static int DistinctColours(Bitmap bitmap)
    {
        using var copy = new WriteableBitmap(bitmap.PixelSize, new Vector(96, 96), Avalonia.Platform.PixelFormat.Rgba8888, Avalonia.Platform.AlphaFormat.Unpremul);
        using var buffer = copy.Lock();
        bitmap.CopyPixels(buffer);
        var seen = new HashSet<int>();
        var row = new int[bitmap.PixelSize.Width];
        for (var y = 0; y < bitmap.PixelSize.Height; y += 2)
        {
            System.Runtime.InteropServices.Marshal.Copy(buffer.Address + (y * buffer.RowBytes), row, 0, row.Length);
            foreach (var pixel in row)
            {
                seen.Add(pixel);
            }
        }

        return seen.Count;
    }

    private static async Task<Bitmap> ShootAsync(string name, DensityMode density, ThemeVariant variant, Avalonia.Media.FlowDirection flow, string state)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        app.RequestedThemeVariant = variant;
        var gateway = new FakeReportGateway
        {
            Description = Fx.Simple(parameters: 3),
            Result = state switch
            {
                "empty" => Fx.Result(rows: 0),
                "truncated" => Fx.Result(rows: 60, truncated: true, summable: true),
                _ => Fx.Result(rows: 60, balances: ["1250.00", "-310.55", "12345678901234567890.12"], summable: true, messages: [new ReportMessage("info", ReportJson.ColumnsRemovedCode, "1 column was removed.")]),
            },
        };
        if (state == "loading")
        {
            gateway.HoldDescribe = new TaskCompletionSource<ReportDescription>().Task;
        }

        var shown = await ViewHarness.ShowAsync(gateway, load: state != "loading", width: 1100, height: 560);
        shown.Window.RequestedThemeVariant = variant;
        shown.Window.FlowDirection = flow;
        Density.SetMode(shown.View, density);
        if (state == "loading")
        {
            _ = shown.Model.LoadAsync();
        }
        else
        {
            ViewHarness.TypeInto(shown.View, "p0", "Acme Trading");
            if (state == "error")
            {
                gateway.FailRun = new ApiRequestException(Fx.Error("api.unavailable", "The server is busy. Press Ctrl+Enter to try again.", 503));
            }

            await shown.View.RunAsync();
            if (state != "error")
            {
                await ViewHarness.SettleAsync(shown.View);
            }
        }

        for (var i = 0; i < 8; i++)
        {
            await Pumping.PumpAsync(25);
        }

        ScreenshotAssert.Capture(shown.Window).Dispose();
        await Pumping.PumpAsync(40);
        var bitmap = ScreenshotAssert.Capture(shown.Window);
        bitmap.Save(Path.Combine(OutputDirectory(), name), new PngBitmapEncoderOptions());
        shown.Window.Close();
        app.RequestedThemeVariant = previous;
        return bitmap;
    }

    [AvaloniaTheory]
    [InlineData("report-comfortable-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "normal")]
    [InlineData("report-compact-light-ltr.png", DensityMode.Compact, false, FlowDirection.LeftToRight, "normal")]
    [InlineData("report-comfortable-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, "normal")]
    [InlineData("report-compact-dark-ltr.png", DensityMode.Compact, true, FlowDirection.LeftToRight, "normal")]
    [InlineData("report-comfortable-light-rtl.png", DensityMode.Comfortable, false, FlowDirection.RightToLeft, "normal")]
    [InlineData("report-compact-dark-rtl.png", DensityMode.Compact, true, FlowDirection.RightToLeft, "normal")]
    [InlineData("report-empty-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "empty")]
    [InlineData("report-empty-dark-rtl.png", DensityMode.Comfortable, true, FlowDirection.RightToLeft, "empty")]
    [InlineData("report-error-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "error")]
    [InlineData("report-loading-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, "loading")]
    [InlineData("report-truncated-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "truncated")]
    [InlineData("report-truncated-dark-rtl.png", DensityMode.Compact, true, FlowDirection.RightToLeft, "truncated")]
    public async Task CaptureTheSet(string name, DensityMode density, bool dark, FlowDirection flow, string state)
    {
        using var bitmap = await ShootAsync(name, density, dark ? ThemeVariant.Dark : ThemeVariant.Light, flow, state);

        Assert.True(File.Exists(Path.Combine(OutputDirectory(), name)));
        Assert.True(DistinctColours(bitmap) > 12, "the capture holds a drawn report, not an empty frame");
    }

    [AvaloniaFact]
    public async Task EachStateLooksDifferent()
    {
        foreach (var (file, state) in new[] { ("cmp-normal.png", "normal"), ("cmp-empty.png", "empty"), ("cmp-error.png", "error"), ("cmp-truncated.png", "truncated"), ("cmp-loading.png", "loading") })
        {
            (await ShootAsync(file, DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, state)).Dispose();
        }

        var hashes = Compared.Select(n => Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(File.ReadAllBytes(Path.Combine(OutputDirectory(), n))))).ToArray();
        Assert.Equal(hashes.Length, hashes.Distinct(StringComparer.Ordinal).Count());
    }
}
