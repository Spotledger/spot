using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Interactivity;
using Avalonia.Media.Imaging;
using Avalonia.VisualTree;
using Spotledger.Designer.Reports;
using Spotledger.Designer.Reports.Views;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

/// <summary>Shows a view in a headless window and finds its controls by name, so a test fails (never skips) when a control is missing.</summary>
internal sealed class Shown<TView, TModel>
    where TView : Control
    where TModel : class
{
    public Shown(TView view, TModel model, Window window)
    {
        View = view;
        Model = model;
        Window = window;
    }

    public TView View { get; }

    public TModel Model { get; }

    public Window Window { get; }

    public T Find<T>(string name)
        where T : Control =>
        View.FindControl<T>(name) ?? throw new Xunit.Sdk.XunitException($"The view has no {typeof(T).Name} named {name}.");

    public IEnumerable<T> All<T>()
        where T : Control => View.GetVisualDescendants().OfType<T>();

    public IEnumerable<string> Texts() => All<TextBlock>().Select(t => t.Text ?? string.Empty);
}

internal static class Clicks
{
    public static void On(Button button)
    {
        Assert.True(button.IsEffectivelyEnabled, $"The {button.Content} button should be enabled.");
        if (button.Command is { } command)
        {
            Assert.True(command.CanExecute(button.CommandParameter), $"The {button.Content} command should be executable.");
            command.Execute(button.CommandParameter);
        }
        else
        {
            button.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        }

        TestHost.Pump();
    }

    /// <summary>Waits for the status line to say something.</summary>
    public static async Task<bool> StatusContainsAsync(TextBlock status, string text) =>
        await Pumping.WaitAsync(() => (status.Text ?? string.Empty).Contains(text, StringComparison.Ordinal));
}

internal static class Views
{
    public static async Task<Shown<ReportBuilderView, ReportBuilderViewModel>> ReportAsync(FakeBuilderServer server)
    {
        var model = new ReportBuilderViewModel(server, "rpt-1");
        await model.OpenAsync();
        var view = new ReportBuilderView { DataContext = model };
        var window = TestHost.Show(view);
        return new Shown<ReportBuilderView, ReportBuilderViewModel>(view, model, window);
    }

    public static async Task<Shown<DashboardBuilderView, DashboardBuilderViewModel>> DashboardAsync(FakeBuilderServer server)
    {
        var model = new DashboardBuilderViewModel(server, "dash-1");
        await model.OpenAsync();
        var view = new DashboardBuilderView { DataContext = model };
        var window = TestHost.Show(view);
        return new Shown<DashboardBuilderView, DashboardBuilderViewModel>(view, model, window);
    }

    public static Shown<ApprovalWizardView, ApprovalWizardViewModel> Wizard(FakeBuilderServer server)
    {
        var model = new ApprovalWizardViewModel(server, "hr");
        var view = new ApprovalWizardView { DataContext = model };
        var window = TestHost.Show(view);
        return new Shown<ApprovalWizardView, ApprovalWizardViewModel>(view, model, window);
    }

    /// <summary>How many distinct colours a rendered frame holds; a blank or failed render holds one or two.</summary>
    public static int DistinctColours(Window window, string name)
    {
        TestHost.Pump();
        var frame = window.CaptureRenderedFrame() ?? throw new Xunit.Sdk.XunitException("The window rendered no frame (is a real rasterizer configured?).");
        var directory = Path.Combine(AppContext.BaseDirectory, "screenshots", "designer-reports");
        Directory.CreateDirectory(directory);
        frame.Save(Path.Combine(directory, name + ".png"), new PngBitmapEncoderOptions());
        using var copy = new WriteableBitmap(frame.PixelSize, new Vector(96, 96), Avalonia.Platform.PixelFormat.Rgba8888, Avalonia.Platform.AlphaFormat.Unpremul);
        using var buffer = copy.Lock();
        frame.CopyPixels(buffer);
        var seen = new HashSet<int>();
        var row = new int[frame.PixelSize.Width];
        for (var y = 0; y < frame.PixelSize.Height; y += 2)
        {
            System.Runtime.InteropServices.Marshal.Copy(buffer.Address + (y * buffer.RowBytes), row, 0, row.Length);
            foreach (var pixel in row)
            {
                seen.Add(pixel);
            }
        }

        return seen.Count;
    }
}
