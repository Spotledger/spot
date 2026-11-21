using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>
/// The screenshot set: comfortable and compact density, light and dark, left to right and right to
/// left, with the totals row, an error marker and an open cell editor. Captures land in this test
/// project's own output folder (never beside the sources), so an ordinary run cannot overwrite a
/// reviewed image; each capture is checked to hold a real picture, not an empty frame.
/// </summary>
public sealed class ScreenshotTests
{
    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "childtable");
        Directory.CreateDirectory(dir);
        return dir;
    }

    private static ChildTableView View() => new(
    [
        new ChildTableColumn("item", Total: ChildTableTotal.Count),
        new ChildTableColumn("desc"),
        new ChildTableColumn("qty", Total: ChildTableTotal.Sum),
        new ChildTableColumn("rate", Total: ChildTableTotal.Average),
        new ChildTableColumn("amount", Total: ChildTableTotal.Sum),
        new ChildTableColumn("unit"),
        new ChildTableColumn("due"),
        new ChildTableColumn("taxable"),
    ]);

    private static Bitmap Shoot(string name, DensityMode density, ThemeVariant variant, FlowDirection flow, bool editing)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        app.RequestedThemeVariant = variant;
        var draft = Fixtures.Draft(12);
        var table = Keys.Show(draft, view: View(), width: 1100, height: 420);
        table.Model.ShowErrors([new FieldError("items[3].qty", "x", "Quantity is too high"), new FieldError("items[5]", "x", "Check this row")]);
        var window = (Window)TopLevel.GetTopLevel(table)!;
        window.RequestedThemeVariant = variant;
        window.FlowDirection = flow;
        Density.SetMode(table, density);
        table.Grid.SetCursor(2, Fixtures.Qty);
        table.Grid.SetSelection([2]);
        table.Grid.Focus();
        TestHost.Pump();
        if (editing)
        {
            Keys.Press(table, Key.F2);
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
    [InlineData("table-comfortable-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, false)]
    [InlineData("table-compact-light-ltr.png", DensityMode.Compact, false, FlowDirection.LeftToRight, false)]
    [InlineData("table-comfortable-dark-ltr.png", DensityMode.Comfortable, true, FlowDirection.LeftToRight, false)]
    [InlineData("table-compact-dark-ltr.png", DensityMode.Compact, true, FlowDirection.LeftToRight, false)]
    [InlineData("table-comfortable-light-rtl.png", DensityMode.Comfortable, false, FlowDirection.RightToLeft, false)]
    [InlineData("table-compact-dark-rtl.png", DensityMode.Compact, true, FlowDirection.RightToLeft, false)]
    [InlineData("table-editing-light-ltr.png", DensityMode.Comfortable, false, FlowDirection.LeftToRight, true)]
    [InlineData("table-editing-dark-rtl.png", DensityMode.Compact, true, FlowDirection.RightToLeft, true)]
    public void CaptureTheSet(string name, DensityMode density, bool dark, FlowDirection flow, bool editing)
    {
        using var bitmap = Shoot(name, density, dark ? ThemeVariant.Dark : ThemeVariant.Light, flow, editing);
        Assert.True(File.Exists(Path.Combine(OutputDirectory(), name)));
        Assert.True(DistinctColours(bitmap) > 12, "the capture holds a drawn table, not an empty frame");
    }

    [AvaloniaFact]
    public void AnOpenEditorChangesThePicture()
    {
        using var plain = Shoot("table-compare-plain.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, false);
        using var editing = Shoot("table-compare-editing.png", DensityMode.Comfortable, ThemeVariant.Light, FlowDirection.LeftToRight, true);
        var a = File.ReadAllBytes(Path.Combine(OutputDirectory(), "table-compare-plain.png"));
        var b = File.ReadAllBytes(Path.Combine(OutputDirectory(), "table-compare-editing.png"));
        Assert.NotEqual(a, b);
    }
}
