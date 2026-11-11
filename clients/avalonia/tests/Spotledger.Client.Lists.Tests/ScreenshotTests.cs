using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Controls;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

/// <summary>The screenshot set: comfortable and compact density, light and dark, left to right and right to
/// left, and the empty, error and loading states. Captures land in this test project's own output folder
/// (<c>screenshots/lists</c>), and each is checked to hold a real picture, not an empty frame.</summary>
public sealed class ScreenshotTests
{
    private static readonly string[] Compared = ["cmp-normal.png", "cmp-empty.png", "cmp-error.png"];

    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "lists");
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

    private static async Task<Bitmap> ShootAsync(string name, DensityMode density, ThemeVariant variant, FlowDirection flow, string state)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        app.RequestedThemeVariant = variant;
        var gateway = new FakeListGateway(state == "empty" ? 0 : 400);
        var settle = state is not ("loading" or "error");
        if (state == "loading")
        {
            gateway.Hold = (_, _, token) => new TaskCompletionSource().Task.WaitAsync(token);
        }

        var s = await ViewHarness.ShowAsync(gateway, width: 1100, height: 560, settle: settle);
        s.Window.RequestedThemeVariant = variant;
        s.Window.FlowDirection = flow;
        Density.SetMode(s.View, density);
        if (state == "filtered")
        {
            s.Model.AddCondition(new FilterCondition("status", FilterOperator.OneOf, ["Open"]));
            s.Model.AddCondition(new FilterCondition("amount", FilterOperator.From, ["1000"]));
            await ViewHarness.SettleAsync(s.View);
        }

        if (state == "empty")
        {
            await s.Model.Source!.GetWindow(0, 10, TestContext.Current.CancellationToken);
        }

        if (state == "error")
        {
            gateway.FailNext = new Api.ApiRequestException(Fixtures.Error("api.unavailable", "The server is busy. Press F5 to try again.", 503));
            s.Model.Refresh();
        }

        for (var i = 0; i < 8; i++)
        {
            await ViewHarness.PumpAsync(25);
        }

        ScreenshotAssert.Capture(s.Window).Dispose();
        await ViewHarness.PumpAsync(40);
        var bitmap = ScreenshotAssert.Capture(s.Window);
        bitmap.Save(Path.Combine(OutputDirectory(), name), new PngBitmapEncoderOptions());
        s.Window.Close();
        app.RequestedThemeVariant = previous;
        return bitmap;
    }

    [AvaloniaTheory]
    [InlineData("list-comfortable-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "normal")]
    [InlineData("list-compact-light-ltr.png", DensityMode.Compact, false, FlowDirection.LeftToRight, "normal")]
    [InlineData("list-comfortable-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, "normal")]
    [InlineData("list-compact-dark-ltr.png", DensityMode.Compact, true, FlowDirection.LeftToRight, "normal")]
    [InlineData("list-comfortable-light-rtl.png", DensityMode.Comfortable, false, FlowDirection.RightToLeft, "normal")]
    [InlineData("list-compact-dark-rtl.png", DensityMode.Compact, true, FlowDirection.RightToLeft, "normal")]
    [InlineData("list-filtered-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "filtered")]
    [InlineData("list-empty-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "empty")]
    [InlineData("list-empty-dark-rtl.png", DensityMode.Comfortable, true, FlowDirection.RightToLeft, "empty")]
    [InlineData("list-error-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "error")]
    [InlineData("list-loading-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, "loading")]
    public async Task CaptureTheSet(string name, DensityMode density, bool dark, FlowDirection flow, string state)
    {
        using var bitmap = await ShootAsync(name, density, dark ? ThemeVariant.Dark : ThemeVariant.Light, flow, state);

        Assert.True(File.Exists(Path.Combine(OutputDirectory(), name)));
        Assert.True(DistinctColours(bitmap) > 12, "the capture holds a drawn list, not an empty frame");
    }

    [AvaloniaFact]
    public async Task EachStateLooksDifferent()
    {
        using var normal = await ShootAsync("cmp-normal.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, "normal");
        using var empty = await ShootAsync("cmp-empty.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, "empty");
        using var error = await ShootAsync("cmp-error.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, "error");

        var bytes = Compared.Select(n => File.ReadAllBytes(Path.Combine(OutputDirectory(), n))).ToArray();
        Assert.NotEqual(bytes[0], bytes[1]);
        Assert.NotEqual(bytes[1], bytes[2]);
        Assert.NotEqual(bytes[0], bytes[2]);
    }
}
