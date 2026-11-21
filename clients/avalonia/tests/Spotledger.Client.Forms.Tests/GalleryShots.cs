using System.Text.Json.Nodes;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

/// <summary>
/// A generator, not a test of pixels: it lays out a realistic Sales Order form from DocType
/// metadata (sections, text, decimal, date, link, select, checkbox, attachment, long text and a
/// child table, with a server validation error and permission-driven read-only fields) and writes
/// PNGs to the directory named by <c>SPOT_SCREENSHOT_DIR</c>. It never compares against a
/// baseline and does nothing when the variable is unset, so ordinary runs are unaffected. The
/// compile lane job <c>ui_screenshots</c> runs it and uploads the artifact <c>ui-screenshots</c>.
/// </summary>
public sealed class GalleryShots
{
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

    private static string L(bool urdu, string english, string urduText) => urdu ? urduText : english;

    internal static DocTypeModel SalesOrderModel(bool urdu = false)
    {
        var line = new DocTypeModel("line", L(urdu, "Order line", "آرڈر لائن"), 1, false,
        [
            new FieldModel("item", L(urdu, "Item", "آئٹم"), FieldKind.Text, required: true),
            new FieldModel("qty", L(urdu, "Qty", "مقدار"), FieldKind.DecimalNumber, scale: 2, precision: 18, required: true),
            new FieldModel("rate", L(urdu, "Rate", "نرخ"), FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("amount", L(urdu, "Amount", "رقم"), FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true),
        ]);
        return new DocTypeModel("invoice", L(urdu, "Sales Order", "سیلز آرڈر"), 1, true,
        [
            new FieldModel("customer", L(urdu, "Customer", "گاہک"), FieldKind.Link, linkTo: "party", required: true),
            new FieldModel("po_number", L(urdu, "Customer PO number", "گاہک پی او نمبر"), FieldKind.Text, required: true),
            new FieldModel("posting_date", L(urdu, "Posting date", "تاریخ اندراج"), FieldKind.Date, required: true),
            new FieldModel("delivery_date", L(urdu, "Delivery date", "تاریخ ترسیل"), FieldKind.Date),
            new FieldModel("currency", L(urdu, "Currency", "کرنسی"), FieldKind.Select, options: ["PKR", "USD", "EUR"], defaultValue: new SelectFieldValue("PKR")),
            new FieldModel("urgent", L(urdu, "Urgent delivery", "فوری ترسیل"), FieldKind.Bool),
            new FieldModel("items", L(urdu, "Items", "اشیا"), FieldKind.Table, childDoctype: "line"),
            new FieldModel("net_total", L(urdu, "Net total", "خالص کل"), FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("tax", L(urdu, "Sales tax (18%)", "سیلز ٹیکس"), FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("grand_total", L(urdu, "Grand total", "مجموعی کل"), FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("approval", L(urdu, "Approval status", "منظوری"), FieldKind.Select, options: ["Pending", "Approved", "Rejected"]),
            new FieldModel("contract", L(urdu, "Signed contract", "دستخط شدہ معاہدہ"), FieldKind.Attach),
            new FieldModel("notes", L(urdu, "Internal notes", "اندرونی نوٹس"), FieldKind.LongText),
        ],
        new Dictionary<string, DocTypeModel> { ["items"] = line });
    }

    internal static FormView SalesOrderLayout(bool urdu = false) => new(
    [
        new FormViewTab(L(urdu, "Sales Order", "سیلز آرڈر"),
        [
            new FormViewSection(L(urdu, "Customer and dates", "گاہک اور تاریخیں"), ["customer", "po_number", "posting_date", "delivery_date", "currency", "urgent"]),
            new FormViewSection(L(urdu, "Items", "اشیا"), ["items"]),
            new FormViewSection(L(urdu, "Totals and approval", "کل اور منظوری"), ["net_total", "tax", "grand_total", "approval", "contract", "notes"]),
        ]),
    ]);

    private static FakeGateway Gateway(bool urdu, string state)
    {
        var gateway = new FakeGateway
        {
            Model = SalesOrderModel(urdu),
            // grand_total and approval are read-only for this user: a permissioned field.
            Snapshot = Fixtures.Permissions("""{"read_only":["grand_total","approval"]}"""),
        };
        gateway.Documents["SO-0042"] = new JsonObject
        {
            ["id"] = "SO-0042",
            ["version"] = 3,
            ["_state"] = state,
            ["_created_by"] = "ama",
            ["_created_at"] = "2026-11-18T09:00:00Z",
            ["_modified_by"] = "ben",
            ["_modified_at"] = "2026-11-20T10:30:00Z",
            ["customer"] = "CUST-1",
            ["po_number"] = "PO-88231",
            ["posting_date"] = "2026-11-21",
            ["delivery_date"] = "2026-12-05",
            ["currency"] = "PKR",
            ["urgent"] = true,
            ["net_total"] = "1424500.00",
            ["tax"] = "256410.00",
            ["grand_total"] = "1680910.00",
            ["approval"] = "Pending",
            ["contract"] = "file-2026-0117",
            ["notes"] = "Deliver to the Karachi warehouse, gate 3. Confirm the dispatch window with the customer on Monday.",
            ["items"] = new JsonArray
            {
                new JsonObject { ["id"] = "r1", ["item"] = "Galvanised sheet 0.5 mm", ["qty"] = "120.00", ["rate"] = "4850.00", ["amount"] = "582000.00" },
                new JsonObject { ["id"] = "r2", ["item"] = "Steel pipe 2 in", ["qty"] = "300.00", ["rate"] = "1975.00", ["amount"] = "592500.00" },
                new JsonObject { ["id"] = "r3", ["item"] = "Angle bar 40x40", ["qty"] = "80.00", ["rate"] = "3125.00", ["amount"] = "250000.00" },
            },
        };
        return gateway;
    }

    private static async Task ShootAsync(string directory, string name, bool dark, bool rtl, bool errors, string state = "draft", int width = 1280, int height = 900)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        var variant = dark ? ThemeVariant.Dark : ThemeVariant.Light;
        app.RequestedThemeVariant = variant;
        var gateway = Gateway(rtl, state);
        var form = Fixtures.Form(gateway, "SO-0042", new FakeShell());
        form.ViewDefinition = SalesOrderLayout(rtl);
        var view = new DocumentFormView(form);
        var window = TestHost.Show(view, width, height);
        await form.LoadAsync();
        TestHost.Pump();
        window.RequestedThemeVariant = variant;
        window.FlowDirection = rtl ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;
        if (errors)
        {
            form.Draft!.TrySetText("po_number", "PO-88231 (duplicate)");
            gateway.RejectNextWrite = new Api.ApiError
            {
                StatusCode = 422,
                Code = "document.validation_failed",
                Message = "The order could not be saved",
                FieldErrors =
                [
                    new Api.ApiFieldError("po_number", "document.invalid_value", "This PO number is already used by SO-0031"),
                    new Api.ApiFieldError("delivery_date", "document.invalid_value", "Delivery must be after the posting date"),
                    new Api.ApiFieldError("items[1].qty", "document.invalid_value", "Quantity exceeds the stock on hand (240.00)"),
                ],
            };
            await form.SaveAsync();
        }

        for (var i = 0; i < 12; i++)
        {
            TestHost.Pump();
            await Task.Delay(25, TestContext.Current.CancellationToken);
        }

        ScreenshotAssert.Capture(window).Dispose();
        for (var i = 0; i < 4; i++)
        {
            TestHost.Pump();
            await Task.Delay(25, TestContext.Current.CancellationToken);
        }

        using var bitmap = ScreenshotAssert.Capture(window);
        bitmap.Save(Path.Combine(directory, name), new PngBitmapEncoderOptions());
        window.Close();
        app.RequestedThemeVariant = previous;
    }

    [AvaloniaFact]
    public async Task SalesOrderFormPages()
    {
        if (OutputDirectory() is not { } dir)
        {
            return;
        }

        await ShootAsync(dir, "form-sales-order-light.png", dark: false, rtl: false, errors: false);
        await ShootAsync(dir, "form-sales-order-errors-light.png", dark: false, rtl: false, errors: true);
        await ShootAsync(dir, "form-sales-order-posted-light.png", dark: false, rtl: false, errors: false, state: "posted");
        await ShootAsync(dir, "form-sales-order-dark.png", dark: true, rtl: false, errors: true);
        await ShootAsync(dir, "form-sales-order-urdu-rtl-light.png", dark: false, rtl: true, errors: true);
        await ShootAsync(dir, "form-sales-order-urdu-rtl-dark.png", dark: true, rtl: true, errors: false);
        Assert.Equal(6, Directory.GetFiles(dir, "form-sales-order-*.png").Length);
    }
}
