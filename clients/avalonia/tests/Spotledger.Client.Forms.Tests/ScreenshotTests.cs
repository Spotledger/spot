using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Controls;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

/// <summary>
/// The screenshot set: comfortable and compact density, light and dark, left to right and right
/// to left, a saved draft with a server error on a field and a cell, and a posted (read-only)
/// document. Captures land in this test project's own output folder (<c>screenshots/forms</c>,
/// never beside the sources), and each is checked to hold a real picture, not an empty frame.
/// </summary>
public sealed class ScreenshotTests
{
    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "forms");
        Directory.CreateDirectory(dir);
        return dir;
    }

    private static async Task<Bitmap> ShootAsync(string name, DensityMode density, ThemeVariant variant, FlowDirection flow, string state, bool errors)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        app.RequestedThemeVariant = variant;
        var gateway = new FakeGateway().Seed("INV-1", state: state, title: "October supplies");
        gateway.Documents["INV-1"]["customer"] = "CUST-1";
        var (view, form, window) = await FormViewTests.ShowAsync(gateway, "INV-1", width: 1100, height: 760);
        window.RequestedThemeVariant = variant;
        window.FlowDirection = flow;
        Density.SetMode(view, density);
        if (errors)
        {
            form.Draft!.TrySetText("title", "October supplies, revised");
            gateway.RejectNextWrite = new Api.ApiError
            {
                StatusCode = 422,
                Code = "document.validation_failed",
                Message = "Invalid",
                FieldErrors =
                [
                    new Api.ApiFieldError("title", "document.invalid_value", "That title is already used"),
                    new Api.ApiFieldError("items[2].qty", "document.invalid_value", "Quantity is too high"),
                ],
            };
            await form.SaveAsync();
        }

        for (var i = 0; i < 8; i++)
        {
            // The grid fetches its window asynchronously: let it land before the picture.
            TestHost.Pump();
            await Task.Delay(25);
        }

        // The first render asks the grid for its rows; the second shows them.
        ScreenshotAssert.Capture(window).Dispose();
        for (var i = 0; i < 4; i++)
        {
            TestHost.Pump();
            await Task.Delay(25);
        }

        var bitmap = ScreenshotAssert.Capture(window);
        bitmap.Save(Path.Combine(OutputDirectory(), name), new PngBitmapEncoderOptions());
        window.Close();
        app.RequestedThemeVariant = previous;
        return bitmap;
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

    [AvaloniaTheory]
    [InlineData("form-comfortable-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "draft", false)]
    [InlineData("form-compact-light-ltr.png", DensityMode.Compact, false, FlowDirection.LeftToRight, "draft", false)]
    [InlineData("form-comfortable-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, "draft", false)]
    [InlineData("form-compact-dark-ltr.png", DensityMode.Compact, true, FlowDirection.LeftToRight, "draft", false)]
    [InlineData("form-comfortable-light-rtl.png", DensityMode.Comfortable, false, FlowDirection.RightToLeft, "draft", false)]
    [InlineData("form-compact-dark-rtl.png", DensityMode.Compact, true, FlowDirection.RightToLeft, "draft", false)]
    [InlineData("form-errors-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, "draft", true)]
    [InlineData("form-posted-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, "posted", false)]
    public async Task CaptureTheSet(string name, DensityMode density, bool dark, FlowDirection flow, string state, bool errors)
    {
        using var bitmap = await ShootAsync(name, density, dark ? ThemeVariant.Dark : ThemeVariant.Light, flow, state, errors);

        Assert.True(File.Exists(Path.Combine(OutputDirectory(), name)));
        Assert.True(DistinctColours(bitmap) > 12, "the capture holds a drawn form, not an empty frame");
    }

    [AvaloniaFact]
    public async Task AnErrorChangesThePicture()
    {
        using var plain = await ShootAsync("form-compare-plain.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, "draft", false);
        using var failed = await ShootAsync("form-compare-errors.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, "draft", true);

        Assert.NotEqual(
            File.ReadAllBytes(Path.Combine(OutputDirectory(), "form-compare-plain.png")),
            File.ReadAllBytes(Path.Combine(OutputDirectory(), "form-compare-errors.png")));
    }
}
