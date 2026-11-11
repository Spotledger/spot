using System.IO;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Screenshot parity (brief §8): renders the real <see cref="ShellView"/> at 1920×1080 headless
/// with Skia and commits a PNG per mockup panel into <c>clients/avalonia/docs/shell-screenshots/</c>.
/// Every state is driven through the viewmodel or the native input pipeline (dispatcher, keymap,
/// menu clicks) — never through pixel hacks — so a regenerated PNG is evidence about the same
/// code the Desktop host runs. Re-run with:
/// <c>dotnet test clients/avalonia/tests/Spotledger.Client.Shell.Tests --filter FullyQualifiedName~ScreenshotParityTests</c>.
/// </summary>
public sealed class ScreenshotParityTests : IDisposable
{
    /// <summary>The committed screenshot directory (repo: clients/avalonia/docs/shell-screenshots).</summary>
    private static readonly string OutputDirectory = ResolveOutputDirectory();

    private readonly ApiConnection _connection = new(new FakeServerHandler());
    private Window? _window;

    public void Dispose()
    {
        _window?.Close();
        _connection.Dispose();
    }

    private static string ResolveOutputDirectory()
    {
        // AppContext.BaseDirectory = .../clients/avalonia/tests/Spotledger.Client.Shell.Tests/bin/<cfg>/<tfm>/
        var projectDirectory = Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "..", ".."));
        var docsDirectory = Path.GetFullPath(Path.Combine(projectDirectory, "..", "..", "docs", "shell-screenshots"));
        if (!string.Equals(Path.GetFileName(Path.GetDirectoryName(projectDirectory)), "tests", StringComparison.Ordinal))
        {
            throw new InvalidOperationException($"Unexpected test project location '{projectDirectory}'.");
        }

        Directory.CreateDirectory(docsDirectory);
        return docsDirectory;
    }

    private static void Pump()
    {
        for (var round = 0; round < 3; round++)
        {
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }
    }

    /// <summary>Waits (by pumping the dispatcher, not by sleeping on a guess) until a condition holds.</summary>
    private static void PumpUntil(Func<bool> condition, string because)
    {
        for (var attempt = 0; attempt < 60 && !condition(); attempt++)
        {
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }

        Assert.True(condition(), $"Timed out waiting for {because}.");
    }

    private async Task<(Window Window, ShellView View, ShellViewModel Shell)> ShowShellAsync(
        bool withSessionHeader = true)
    {
        var shell = new ShellViewModel(_connection);
        await shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        if (withSessionHeader)
        {
            // The header (user chip, site) is session data — loaded exactly like the real app does.
            _connection.Connect(new Uri(TestHarness.ServerUrl));
            _connection.Tokens.SetToken(TestHarness.SessionToken);
            await shell.InitializeAsync(TestContext.Current.CancellationToken);
            shell.NotificationCount = 3;
        }

        var view = new ShellView { DataContext = shell };
        _window = new Window { Content = view, Width = 1920, Height = 1080 };
        _window.Show();
        Pump();
        return (_window, view, shell);
    }

    /// <summary>Captures the rendered window frame (popups included: they live in the window's
    /// own overlay layer under the headless Skia platform) and writes the PNG to the docs folder.</summary>
    private static void Capture(Window window, string fileName)
    {
        Bitmap? frame = null;
        for (var attempt = 0; attempt < 40 && frame is null; attempt++)
        {
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            frame = window.CaptureRenderedFrame();
        }

        Assert.NotNull(frame);
        var path = Path.Combine(OutputDirectory, fileName);
        frame!.Save(path, new PngBitmapEncoderOptions());
        frame.Dispose();
        Assert.Equal(1920, new Bitmap(path).PixelSize.Width);
        Assert.Equal(1080, new Bitmap(path).PixelSize.Height);
        Assert.True(new FileInfo(path).Length > 5_000, $"{fileName} looks empty for a full chrome render.");
    }

    private static Point Center(Window window, Control control) =>
        control.TranslatePoint(new Point(control.Bounds.Width / 2, control.Bounds.Height / 2), window)!.Value;

    private static void Click(Window window, Control control)
    {
        var point = Center(window, control);
        window.MouseDown(point, MouseButton.Left);
        window.MouseUp(point, MouseButton.Left);
        Pump();
    }

    private static MenuItem TopMenu(ShellView view, string label) =>
        view.MenuBar!.GetVisualDescendants()
            .OfType<MenuItem>()
            .Single(item => Equals(item.Header?.ToString(), label) && item.IsTopLevel);

    private static Border TabChip(ShellView view, string name) =>
        view.TabStripControl.GetVisualDescendants().OfType<Border>()
            .Single(chip => chip.Name == name);

    /// <summary>Opens Home plus a pinned tab, a dirty document tab and a plain tab so the
    /// strip shows active/join, dirty dot, pin and close affordances at once.</summary>
    private static async Task SeedTabsAsync(ShellViewModel shell)
    {
        await shell.OpenTabAsync("Buying", TestContext.Current.CancellationToken);
        shell.OpenDocumentTab("Buying", Guid.Parse("8f14e45f-e1b1-4b5a-9b0e-1c2d3e4f5a6b"),
            "Purchase Order · PO-1042", "PO-1042").IsDirty = true;
        var selling = new WorkspaceTabViewModel("Selling") { IsPinned = true };
        shell.Tabs.Add(selling);
        shell.SelectedTab = shell.Tabs[1];
    }

    [AvaloniaFact]
    public async Task Capture01HomeTabs()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        Pump();
        Assert.Equal(4, shell.Tabs.Count);
        Capture(window, "01-home-tabs.png");
    }

    [AvaloniaFact]
    public async Task Capture02MenuTransactions()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);

        // Real click pipeline: Transactions opens, Right enters the highlighted Purchasing
        // submenu — the same route the keyboard tests exercise.
        Click(window, TopMenu(view, "_Transactions"));
        PumpUntil(() => TopMenu(view, "_Transactions").IsSubMenuOpen, "the Transactions dropdown");
        // Click-opening leaves nothing highlighted; Down highlights the first item
        // (Purchasing) exactly as a keyboard user does, then Right enters the submenu.
        window.KeyPressQwerty(PhysicalKey.ArrowDown, RawInputModifiers.None);
        var purchasing = window.GetVisualDescendants().OfType<MenuItem>()
            .Single(item => Equals(item.Header?.ToString(), "_Purchasing"));
        PumpUntil(() => purchasing.IsSelected, "Purchasing to be highlighted");
        window.KeyPressQwerty(PhysicalKey.ArrowRight, RawInputModifiers.None);
        PumpUntil(() => purchasing.IsSubMenuOpen, "the Purchasing submenu");
        Capture(window, "02-menu-transactions.png");
    }

    [AvaloniaFact]
    public async Task Capture03Launcher()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        view.SearchBoxControl.Focus();
        Pump();
        window.KeyPressQwerty(PhysicalKey.K, RawInputModifiers.Control);
        Pump();
        Assert.True(shell.IsLauncherOpen);
        Assert.True(view.LauncherPanelControl.IsVisible);
        Assert.NotNull(shell.Launcher.Selected);
        // The highlighted row exists and carries the Enter badge (brief §6): assert it in
        // the visual tree before capturing, so the PNG evidence is machine-checked.
        Assert.True(shell.Launcher.Selected!.Availability.IsAvailable,
            "the default highlight skips blocked rows");
        var selectedRow = view.LauncherResultsPanel.GetVisualDescendants().OfType<Border>()
            .Single(row => row.Classes.Contains("launcherRowSelected"));
        Assert.Contains(selectedRow.GetVisualDescendants().OfType<TextBlock>(), text => text.Text == "Enter");
        // And it is inside the clipped panel viewport, so the PNG shows the badge.
        var rowTopInPanel = selectedRow.TranslatePoint(new Point(0, 0), view.LauncherPanelControl)!.Value.Y;
        Assert.True(rowTopInPanel >= 0 && rowTopInPanel + selectedRow.Bounds.Height <= view.LauncherPanelControl.Bounds.Height,
            $"selected row at y {rowTopInPanel} is outside the palette viewport");
        Capture(window, "03-launcher.png");
    }

    [AvaloniaFact]
    public async Task Capture04SidebarExpanded()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        shell.Sidebar = SidebarMode.Expanded;
        Pump();
        Assert.Equal(SidebarMode.Expanded, shell.Sidebar);
        Capture(window, "04-sidebar-expanded.png");
    }

    [AvaloniaFact]
    public async Task Capture05SidebarCollapsed()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        shell.Sidebar = SidebarMode.IconsOnly;
        Pump();
        Capture(window, "05-sidebar-collapsed.png");
    }

    [AvaloniaFact]
    public async Task Capture06TabMenu()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        var chip = TabChip(view, "Tab_workspace:Selling");
        var flyout = Assert.IsAssignableFrom<MenuFlyout>(chip.ContextFlyout);
        flyout.ShowAt(chip);
        Pump();
        Assert.True(flyout.IsOpen);
        Capture(window, "06-tab-menu.png");
        flyout.Hide();
    }

    [AvaloniaFact]
    public async Task Capture07Rtl()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        shell.RightToLeft = true;
        Pump();
        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, view.FlowDirection);
        Capture(window, "07-rtl.png");
    }

    [AvaloniaFact]
    public async Task Capture08Dark()
    {
        var application = Application.Current;
        Assert.NotNull(application);
        var previous = application!.RequestedThemeVariant;
        try
        {
            var (window, view, shell) = await ShowShellAsync();
            await SeedTabsAsync(shell);
            application.RequestedThemeVariant = ThemeVariant.Dark;
            Pump();
            Capture(window, "08-dark.png");
        }
        finally
        {
            application.RequestedThemeVariant = previous;
        }
    }

    /// <summary>Brief §5 names three sidebar states; 05 shows icons-only, this one hidden
    /// with the restore toggle visible in the tab strip.</summary>
    [AvaloniaFact]
    public async Task Capture09SidebarHidden()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        shell.Sidebar = SidebarMode.Hidden;
        Pump();
        var restore = NameScope.GetNameScope(view)!.Get<Button>("SidebarRestoreButton")!;
        Assert.True(restore.IsVisible, "hidden sidebar keeps a visible way back");
        Capture(window, "09-sidebar-hidden.png");
    }

    /// <summary>Compact density (brief §7) alongside the Comfortable shots.</summary>
    [AvaloniaFact]
    public async Task Capture10CompactDensity()
    {
        var (window, view, shell) = await ShowShellAsync();
        await SeedTabsAsync(shell);
        shell.CompactDensity = true;
        Pump();
        Capture(window, "10-compact-density.png");
    }
}
