using System.Text.Json;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Grid;
using Spotledger.Client.Lists;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

/// <summary>
/// A generator, not a test of pixels: it shows the Sales Order list (grid columns from DocType
/// metadata, rows, a filter bar, a sorted column and a multi-row selection) and writes PNGs to
/// the directory named by <c>SPOT_SCREENSHOT_DIR</c>. It does nothing when the variable is unset.
/// The compile lane job <c>ui_screenshots</c> runs it and uploads the artifact <c>ui-screenshots</c>.
/// </summary>
public sealed class GalleryShots
{
    private static readonly string[] Customers =
        ["Al-Noor Traders", "Karachi Steel Works", "Bilal Hardware", "Pak Fasteners Ltd", "Indus Construction", "Lahore Tube Mills", "Hamid and Sons"];

    private static readonly string[] Statuses = ["Open", "Paid", "Open", "Void", "Open", "Paid"];

    private sealed class SalesGateway : FakeListGateway
    {
        public SalesGateway(int rows) : base(rows) { }

        public override ListRecord Record(int i, IEnumerable<string> fields)
        {
            var record = base.Record(i, fields);
            var values = new Dictionary<string, JsonElement>(record.Values, StringComparer.Ordinal);
            foreach (var field in fields)
            {
                var json = field switch
                {
                    "title" => $"\"SO-{40 + i:D4}\"",
                    "customer" => $"\"p-{i % 7}\"",
                    "amount" => $"\"{950_000 - i * 2_150}.00\"",
                    "status" => $"\"{Statuses[i % Statuses.Length]}\"",
                    _ => null,
                };
                if (json is not null)
                {
                    values[field] = JsonDocument.Parse(json).RootElement.Clone();
                }
            }

            return record with { Values = values };
        }

        public override Task<IReadOnlyDictionary<string, string>> ResolveLabelsAsync(string targetDoctype, IReadOnlyCollection<string> ids, CancellationToken cancellationToken)
            => Task.FromResult<IReadOnlyDictionary<string, string>>(ids.ToDictionary(id => id, id => Customers[int.Parse(id[2..], System.Globalization.CultureInfo.InvariantCulture) % Customers.Length]));
    }

    private static string? OutputDirectory()
    {
        var dir = Environment.GetEnvironmentVariable("SPOT_SCREENSHOT_DIR");
        if (string.IsNullOrWhiteSpace(dir))
        {
            return null;
        }

        Directory.CreateDirectory(dir);
        return dir;
    }

    private static async Task ShootAsync(string directory, string name, bool dark, bool rtl, bool filtered)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        var variant = dark ? ThemeVariant.Dark : ThemeVariant.Light;
        app.RequestedThemeVariant = variant;
        var s = await ViewHarness.ShowAsync(new SalesGateway(400), width: 1280, height: 800);
        s.Window.RequestedThemeVariant = variant;
        s.Window.FlowDirection = rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;
        if (filtered)
        {
            s.Model.AddCondition(new FilterCondition("status", FilterOperator.OneOf, ["Open"]));
            s.Model.AddCondition(new FilterCondition("amount", FilterOperator.From, ["100000"]));
            await ViewHarness.SettleAsync(s.View);
        }

        // Amount, descending: the sort arrow shows in the header.
        await s.Model.Source!.Sort(2, GridSortDirection.Descending);
        await ViewHarness.SettleAsync(s.View);
        s.View.Grid.SetSelection([1, 2, 4]);
        for (var i = 0; i < 12; i++)
        {
            await ViewHarness.PumpAsync(25);
        }

        ScreenshotAssert.Capture(s.Window).Dispose();
        await ViewHarness.PumpAsync(60);
        using var bitmap = ScreenshotAssert.Capture(s.Window);
        bitmap.Save(Path.Combine(directory, name), new PngBitmapEncoderOptions());
        s.Window.Close();
        app.RequestedThemeVariant = previous;
    }

    [AvaloniaFact]
    public async Task SalesOrderListPages()
    {
        if (OutputDirectory() is not { } dir)
        {
            return;
        }

        await ShootAsync(dir, "list-sales-orders-light.png", dark: false, rtl: false, filtered: false);
        await ShootAsync(dir, "list-sales-orders-filtered-light.png", dark: false, rtl: false, filtered: true);
        await ShootAsync(dir, "list-sales-orders-dark.png", dark: true, rtl: false, filtered: true);
        await ShootAsync(dir, "list-sales-orders-rtl-light.png", dark: false, rtl: true, filtered: false);
        Assert.Equal(4, Directory.GetFiles(dir, "list-sales-orders-*.png").Length);
    }
}
