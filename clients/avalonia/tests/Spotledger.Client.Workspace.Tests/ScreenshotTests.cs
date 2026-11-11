using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Spotledger.Client.Metadata;
using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

/// <summary>
/// The screenshot set for the recovery bar (offering, and reviewing a changed server version) and the
/// "Recover drafts" list, in light and dark, left to right and right to left. Captures land in this
/// project's own output folder (<c>screenshots/workspace</c>); each is checked to hold a real picture.
/// </summary>
public sealed class ScreenshotTests
{
    private static string OutputDirectory()
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "screenshots", "workspace");
        _ = Directory.CreateDirectory(dir);
        return dir;
    }

    private static async Task<RecoveryBarViewModel> BarAsync(bool review)
    {
        var store = new InMemoryDraftStore();
        var edited = Models.LoadedInvoice();
        edited.Set("title", new TextFieldValue("My edit"));
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(edited, key, new DateTimeOffset(2026, 10, 23, 9, 41, 0, TimeSpan.Zero)));
        var draft = review ? Models.LoadedInvoice(version: 4, title: "Edited on the server") : Models.LoadedInvoice();
        var bar = new RecoveryBarViewModel(store, (await store.LoadAsync(key))!, draft, formatTime: t => t.ToString("HH:mm", System.Globalization.CultureInfo.InvariantCulture));
        if (review)
        {
            bar.RestoreCommand.Execute(null);
            await bar.RestoreCommand.ExecutionTask;
        }

        return bar;
    }

    private static async Task<RecoverDraftsViewModel> ListAsync()
    {
        var store = new InMemoryDraftStore();
        var draft = Models.LoadedInvoice();
        draft.Set("title", new TextFieldValue("My edit"));
        _ = await store.SaveAsync(DraftSnapshot.Capture(draft, DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), new DateTimeOffset(2026, 10, 23, 9, 41, 0, TimeSpan.Zero), "October"));
        var fresh = new DocumentDraft(Models.Invoice());
        fresh.Set("title", new TextFieldValue("Unsaved new invoice"));
        _ = await store.SaveAsync(DraftSnapshot.Capture(fresh, DraftKey.ForNew(Models.Scope, "invoice"), new DateTimeOffset(2026, 10, 23, 10, 5, 0, TimeSpan.Zero), "Unsaved new invoice"));
        var list = new RecoverDraftsViewModel(store, Models.Scope, _ => Task.CompletedTask, _ => "Invoice", t => t.ToString("d MMM HH:mm", System.Globalization.CultureInfo.InvariantCulture));
        await list.LoadAsync();
        return list;
    }

    private static Bitmap Shoot(string name, Control view, object viewModel, bool dark, FlowDirection flow, double height)
    {
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        app.RequestedThemeVariant = dark ? ThemeVariant.Dark : ThemeVariant.Light;
        view.DataContext = viewModel;
        var window = TestHost.Show(view, 760, height);
        window.RequestedThemeVariant = app.RequestedThemeVariant;
        window.FlowDirection = flow;
        TestHost.Pump();
        _ = ScreenshotAssert.Capture(window);
        TestHost.Pump();
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
                _ = seen.Add(pixel);
            }
        }

        return seen.Count;
    }

    [AvaloniaTheory]
    [InlineData("bar-offer-light-ltr.png", false, false, FlowDirection.LeftToRight)]
    [InlineData("bar-offer-dark-ltr.png", false, true, FlowDirection.LeftToRight)]
    [InlineData("bar-offer-light-rtl.png", false, false, FlowDirection.RightToLeft)]
    [InlineData("bar-offer-dark-rtl.png", false, true, FlowDirection.RightToLeft)]
    [InlineData("bar-review-light-ltr.png", true, false, FlowDirection.LeftToRight)]
    [InlineData("bar-review-dark-rtl.png", true, true, FlowDirection.RightToLeft)]
    public async Task The_recovery_bar_set(string name, bool review, bool dark, FlowDirection flow)
    {
        var bar = await BarAsync(review);
        using var bitmap = Shoot(name, new RecoveryBarView(), bar, dark, flow, review ? 220 : 120);

        Assert.True(File.Exists(Path.Combine(OutputDirectory(), name)));
        Assert.True(DistinctColours(bitmap) > 8, "the capture holds a drawn bar, not an empty frame");
    }

    [AvaloniaTheory]
    [InlineData("recover-list-light-ltr.png", false, FlowDirection.LeftToRight)]
    [InlineData("recover-list-dark-ltr.png", true, FlowDirection.LeftToRight)]
    [InlineData("recover-list-light-rtl.png", false, FlowDirection.RightToLeft)]
    [InlineData("recover-list-dark-rtl.png", true, FlowDirection.RightToLeft)]
    public async Task The_recover_list_set(string name, bool dark, FlowDirection flow)
    {
        var list = await ListAsync();
        using var bitmap = Shoot(name, new RecoverDraftsView(), list, dark, flow, 260);

        Assert.True(File.Exists(Path.Combine(OutputDirectory(), name)));
        Assert.True(DistinctColours(bitmap) > 8, "the capture holds a drawn list, not an empty frame");
    }

    [AvaloniaFact]
    public async Task The_bar_hides_after_an_answer_and_the_empty_list_says_so()
    {
        var bar = await BarAsync(false);
        var view = new RecoveryBarView { DataContext = bar };
        var window = TestHost.Show(view, 760, 120);
        Assert.True(view.IsVisible);
        bar.DiscardCommand.Execute(null);
        await bar.DiscardCommand.ExecutionTask;
        TestHost.Pump();
        Assert.False(view.IsVisible);
        window.Close();

        var empty = new RecoverDraftsViewModel(new InMemoryDraftStore(), Models.Scope, _ => Task.CompletedTask);
        await empty.LoadAsync();
        Assert.False(empty.HasDrafts);
        Assert.Equal("No unsaved drafts.", empty.EmptyText);
    }
}
