using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The chrome as the user drives it, against the real <see cref="ShellView"/>: the header
/// shows the frame the brief lists, the Ctrl+K palette opens/closes with its reasons and
/// returns focus to the origin, the sidebar cycles expanded/icons-only/hidden, the tab
/// chips carry close and context menu, and Urdu flips the window to right-to-left.
/// </summary>
public sealed class ShellChromeTests : IDisposable
{
    private readonly ApiConnection _connection = new(new FakeServerHandler());
    private Window? _window;

    public void Dispose()
    {
        _window?.Close();
        _connection.Dispose();
    }

    private async Task<(Window Window, ShellView View, ShellViewModel Shell)> ShowShellAsync()
    {
        var shell = new ShellViewModel(_connection);
        await shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        var view = new ShellView { DataContext = shell };
        _window = new Window { Content = view, Width = 1920, Height = 1080 };
        _window.Show();
        Pump();
        return (_window, view, shell);
    }

    private static void Pump()
    {
        Dispatcher.UIThread.RunJobs();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        Dispatcher.UIThread.RunJobs();
    }

    private static Point Center(Window window, Control control) =>
        control.TranslatePoint(new Point(control.Bounds.Width / 2, control.Bounds.Height / 2), window)!.Value;

    private static void Click(Window window, Control control)
    {
        var p = Center(window, control);
        window.MouseDown(p, MouseButton.Left);
        window.MouseUp(p, MouseButton.Left);
        Pump();
    }

    [AvaloniaFact]
    public async Task TheHeaderCarriesLogoMenuBarSearchNewBellAndUserChip()
    {
        var (window, view, shell) = await ShowShellAsync();
        // The header data comes from the session endpoint, not from view code.
        _connection.Connect(new Uri("http://127.0.0.55:8080"));
        _connection.Tokens.SetToken(TestHarness.SessionToken);
        await shell.InitializeAsync(TestContext.Current.CancellationToken);
        shell.NotificationCount = 3;
        Pump();

        var scope = NameScope.GetNameScope(view)!;
        Assert.NotNull(scope.Get<TextBox>("SearchBox"));
        Assert.NotNull(scope.Get<Button>("NewButton"));
        Assert.NotNull(scope.Get<Button>("BellButton"));
        Assert.NotNull(scope.Get<Button>("UserChip"));
        Assert.Equal("Ayesha Khan", scope.Get<TextBlock>("UserNameText")!.Text);
        Assert.Equal("acme.test", scope.Get<TextBlock>("SiteText")!.Text);
        Assert.Equal("AK", scope.Get<TextBlock>("UserInitials")!.Text);
        Assert.True(scope.Get<Border>("BellBadge")!.IsVisible);
        Assert.Equal("3", scope.Get<TextBlock>("BellCount")!.Text);
        Assert.NotNull(view.MenuBar);
        Assert.NotNull(scope.Get<TextBlock>("StatusText"));

        // The New button dispatches the shell.new command: it opens the launcher.
        Assert.False(shell.IsLauncherOpen);
        Click(window, scope.Get<Button>("NewButton")!);
        Assert.True(shell.IsLauncherOpen, "New opens the launcher — no inert primary action");
    }

    [AvaloniaFact]
    public async Task CtrlKOpensThePaletteUnderTheHeaderAndEscapeClosesIt()
    {
        var (window, view, shell) = await ShowShellAsync();
        view.SearchBoxControl.Focus();
        Pump();
        window.KeyPressQwerty(PhysicalKey.K, RawInputModifiers.Control);
        Pump();
        Assert.True(shell.IsLauncherOpen);
        Assert.True(view.LauncherPanelControl.IsVisible);

        // Floating panel under the header: its top edge is below the header row.
        var header = (Border)view.GetVisualDescendants().First(v => v is Border b && b.Classes.Contains("chrome"));
        var launcherTop = view.LauncherPanelControl.TranslatePoint(new Point(0, 0), window)!.Value.Y;
        Assert.True(launcherTop >= header.Bounds.Bottom - 8, $"palette top {launcherTop} vs header bottom {header.Bounds.Bottom}");

        // Grouped results and a highlighted first row.
        Assert.NotEmpty(shell.Launcher.Groups);
        Assert.NotNull(shell.Launcher.Selected);
        var rows = view.GetVisualDescendants().OfType<Border>().Where(b => b.Name?.StartsWith("LauncherRow_", StringComparison.Ordinal) == true).ToList();
        Assert.NotEmpty(rows);
        Assert.Contains(rows, row => row.Classes.Contains("launcherRowSelected"));

        window.KeyPressQwerty(PhysicalKey.Escape, RawInputModifiers.None);
        Pump();
        Assert.False(shell.IsLauncherOpen);
        Assert.False(view.LauncherPanelControl.IsVisible);
    }

    [AvaloniaFact]
    public async Task FocusReturnsToTheOriginAfterTheLauncherCloses()
    {
        var (window, view, shell) = await ShowShellAsync();
        var searchBox = view.SearchBoxControl;
        searchBox.Focus();
        Pump();
        Assert.True(searchBox.IsFocused);

        window.KeyPressQwerty(PhysicalKey.K, RawInputModifiers.Control);
        Pump();
        Assert.True(view.LauncherInputControl.IsFocused, "the palette input takes focus");

        window.KeyPressQwerty(PhysicalKey.Escape, RawInputModifiers.None);
        Pump();
        Assert.True(searchBox.IsFocused, "focus returns to the origin");
    }

    [AvaloniaFact]
    public async Task TypingFiltersAndEnterRunsTheHighlightedCommand()
    {
        var (window, view, shell) = await ShowShellAsync();
        view.Focus();
        Pump();
        window.KeyPressQwerty(PhysicalKey.K, RawInputModifiers.Control);
        Pump();
        Assert.True(shell.IsLauncherOpen, "Ctrl+K opened the palette");
        view.LauncherInputControl.Text = "stock";
        Pump();
        var labels = shell.Launcher.Items.Select(item => item.Label).ToList();
        Assert.Contains("Stock", labels);

        // Enter runs the highlighted row (workspace stock opens a tab).
        window.KeyPressQwerty(PhysicalKey.Enter, RawInputModifiers.None);
        Pump();
        Assert.Contains(shell.Tabs, tab => tab.Workspace == "Stock");
        Assert.False(shell.IsLauncherOpen);
    }

    [AvaloniaFact]
    public async Task ABlockedLauncherRowShowsItsReason()
    {
        var (window, view, shell) = await ShowShellAsync();
        shell.Launcher.Query = "purchase order";
        shell.Launcher.Refresh();
        var entry = shell.Launcher.Items.Single(item => item.Id == "create.purchaseOrder");
        Assert.Equal(AvailabilityState.Blocked, entry.Availability.State);
        Assert.Equal("The form for this document arrives with a later task.", entry.Caption);
    }

    [AvaloniaFact]
    public async Task SidebarCyclesExpandedIconsOnlyAndHiddenWithCtrlB()
    {
        var (window, view, shell) = await ShowShellAsync();
        var scope = NameScope.GetNameScope(view)!;
        var sidebarItems = view.SidebarItemsControl;
        Assert.Equal(8, sidebarItems.GetVisualDescendants().OfType<Border>().Count(row => row.Name?.StartsWith("Sidebar_", StringComparison.Ordinal) == true));
        Assert.True(shell.IsSidebarExpanded);
        var labelsVisible = sidebarItems.GetVisualDescendants().OfType<TextBlock>().Where(t => !string.IsNullOrEmpty(t.Text)).ToList();
        Assert.All(labelsVisible, label => Assert.True(label.IsVisible, $"label {label.Text} visible when expanded"));

        view.Focus();
        Pump();
        window.KeyPressQwerty(PhysicalKey.B, RawInputModifiers.Control);
        Pump();
        Assert.Equal(SidebarMode.IconsOnly, shell.Sidebar);
        Assert.All(labelsVisible, label => Assert.False(label.IsVisible, "labels hide in icons-only"));
        Assert.True(scope.Get<Border>("SidebarBorder")!.IsVisible);

        window.KeyPressQwerty(PhysicalKey.B, RawInputModifiers.Control);
        Pump();
        Assert.Equal(SidebarMode.Hidden, shell.Sidebar);
        Assert.False(scope.Get<Border>("SidebarBorder")!.IsVisible, "hidden removes the sidebar");

        // The visible restore toggle brings it back to expanded.
        var restore = scope.Get<Button>("SidebarRestoreButton")!;
        Assert.True(restore.IsVisible, "a hidden sidebar keeps a visible way back");
        Click(window, restore);
        Assert.Equal(SidebarMode.Expanded, shell.Sidebar);
        Assert.True(scope.Get<Border>("SidebarBorder")!.IsVisible);
    }

    [AvaloniaFact]
    public async Task AWorkspaceRowOpensItsTabAndHighlightsTheSelectedWorkspace()
    {
        var (window, view, shell) = await ShowShellAsync();
        var selling = view.GetVisualDescendants().OfType<Border>().Single(row => row.Name == "Sidebar_Selling");
        Click(window, selling);
        Pump();
        Assert.Equal("Selling", shell.SelectedTab!.Workspace);
        Assert.Equal("Selling", shell.SelectedWorkspace);
        var sellingRow = view.GetVisualDescendants().OfType<Border>().Single(row => row.Name == "Sidebar_Selling");
        Assert.Contains("selected", sellingRow.Classes);
        var homeRow = view.GetVisualDescendants().OfType<Border>().Single(row => row.Name == "Sidebar_Home");
        Assert.DoesNotContain("selected", homeRow.Classes);

        // The unbuilt workspace says in one line what it will hold (in the content area).
        var contentLines = view.TabContentControl.GetVisualDescendants().OfType<TextBlock>().Select(t => t.Text).ToList();
        Assert.Contains(contentLines, line => line!.Contains("arrive", StringComparison.OrdinalIgnoreCase)
            || line!.Contains("later", StringComparison.OrdinalIgnoreCase));
    }

    [AvaloniaFact]
    public async Task TabChipsShowCloseAndOfferTheTabMenu()
    {
        var (window, view, shell) = await ShowShellAsync();
        await shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        Pump();
        var chips = view.TabStripControl.GetVisualDescendants().OfType<Border>().Where(b => b.Name?.StartsWith("Tab_", StringComparison.Ordinal) == true).ToList();
        Assert.Equal(2, chips.Count);
        var active = chips.Single(chip => chip.Classes.Contains("active"));
        var closeButtons = active.GetVisualDescendants().OfType<Button>().Where(b => (string?)b.Content == "×").ToList();
        Assert.Single(closeButtons);

        // The tab chip's flyout offers the brief's menu.
        var sellingChip = chips.Single(chip => chip.Name == "Tab_workspace:Selling");
        Assert.NotNull(sellingChip.ContextFlyout);
        shell.SelectedTab = shell.Tabs[1];
        Pump();
        Assert.Equal("tab.close", shell.Commands["tab.close"].Id);
    }

    [AvaloniaFact]
    public async Task ClosingADirtyTabThroughTheChipAsksSaveDiscardCancel()
    {
        var (window, view, shell) = await ShowShellAsync();
        var tab = shell.Tabs.Single();
        tab.IsDirty = true;
        Pump();
        var closeButton = view.TabStripControl.GetVisualDescendants().OfType<Button>().Single(b => (string?)b.Content == "×");
        Click(window, closeButton);
        Pump();
        Assert.True(view.DialogPanelControl.IsVisible, "the Save/Discard/Cancel dialog opened");
        Assert.Equal(3, view.DialogPanelControl.GetVisualDescendants().OfType<Button>().Count());

        // Click Cancel: the tab stays.
        var cancel = view.DialogPanelControl.GetVisualDescendants().OfType<Button>().Single(b => Equals(b.Content, "Cancel"));
        Click(window, cancel);
        Pump();
        Assert.False(view.DialogPanelControl.IsVisible);
        Assert.Single(shell.Tabs);

        // Click Save: the tab closes (chrome treats Save as clean-and-close).
        var saveClose = view.TabStripControl.GetVisualDescendants().OfType<Button>().Single(b => (string?)b.Content == "×");
        Click(window, saveClose);
        Pump();
        var save = view.DialogPanelControl.GetVisualDescendants().OfType<Button>().Single(b => Equals(b.Content, "Save"));
        Click(window, save);
        Pump();
        Assert.Empty(shell.Tabs);
    }

    [AvaloniaFact]
    public async Task UrduFlipsTheWholeChromeToRightToLeft()
    {
        var (window, view, shell) = await ShowShellAsync();
        Assert.Equal(Avalonia.Media.FlowDirection.LeftToRight, view.FlowDirection);

        shell.RightToLeft = true;
        Pump();
        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, view.FlowDirection);
        // The header, menu bar and sidebar mirror with the flow (inherited).
        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, view.MenuBar!.FlowDirection);
        var status = NameScope.GetNameScope(view)!.Get<TextBlock>("StatusText")!;
        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, status.FlowDirection);
    }

    [AvaloniaFact]
    public async Task DensityAndKeymapShowOnTheStatusBar()
    {
        var (window, view, shell) = await ShowShellAsync();
        var scope = NameScope.GetNameScope(view)!;
        Assert.Equal("Comfortable", scope.Get<TextBlock>("DensityText")!.Text);
        Assert.Equal("Keyboard: desktop", scope.Get<TextBlock>("ProfileText")!.Text);

        shell.CompactDensity = true;
        Pump();
        Assert.Equal("Compact", scope.Get<TextBlock>("DensityText")!.Text);
    }
}
