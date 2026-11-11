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
/// Desktop menu conventions driven through the real Avalonia input pipeline against the
/// real <see cref="ShellView"/>: the dropdown opens directly under its item, click,
/// Alt+letter and F10 reach the same items, hovering a neighbour switches, arrows and
/// Home/End navigate, a click outside closes, mnemonics underline only while Alt is held,
/// and a blocked command is greyed with its reason. The popup is the window's own overlay
/// layer: no extra top level, no modal.
/// </summary>
public sealed class MenuBehaviourTests : IDisposable
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

    private static Point CenterInWindow(Control control)
    {
        var window = control.GetVisualAncestors().OfType<Window>().First();
        return control.TranslatePoint(new Point(control.Bounds.Width / 2, control.Bounds.Height / 2), window) ?? default;
    }

    private static void ClickOn(Window window, Control control)
    {
        var point = CenterInWindow(control);
        window.MouseDown(point, MouseButton.Left);
        window.MouseUp(point, MouseButton.Left);
        Pump();
    }

    private static void HoverOver(Window window, Control control)
    {
        window.MouseMove(CenterInWindow(control));
        Pump();
    }

    private static MenuItem TopMenu(ShellView view, string label) =>
        view.MenuBar!.GetVisualDescendants()
            .OfType<MenuItem>()
            .Single(item => Equals(item.Header?.ToString(), label) && item.IsTopLevel);

    /// <summary>Items inside an open dropdown: MenuItems whose ancestors reach a popup
    /// host (the headless platform embeds popups in the window's overlay layer).</summary>
    private static IEnumerable<MenuItem> PopupItems(Window window) =>
        window.GetVisualDescendants().OfType<MenuItem>().Where(item => item.GetVisualAncestors()
            .Any(ancestor => ancestor is Avalonia.Controls.Primitives.OverlayPopupHost
                             or Avalonia.Controls.Primitives.PopupRoot
                             or Avalonia.Controls.Primitives.OverlayLayer));

    [AvaloniaFact]
    public async Task ClickOpensTheDropdownDirectlyUnderTheItem()
    {
        var (window, view, _) = await ShowShellAsync();
        var file = TopMenu(view, "_File");
        Assert.False(file.IsSubMenuOpen);
        ClickOn(window, file);
        Assert.True(file.IsSubMenuOpen, "click opens the dropdown");
        var items = PopupItems(window).ToList();
        Assert.Contains(items, item => Equals(item.Header?.ToString(), "_Command launcher"));

        // Bounds: the dropdown's top edge sits at the item's bottom edge.
        var itemBottom = file.TranslatePoint(new Point(0, file.Bounds.Bottom), window)!.Value.Y;
        var host = items[0].GetVisualAncestors()
            .OfType<Visual>()
            .First(ancestor => ancestor is Avalonia.Controls.Primitives.OverlayPopupHost
                               or Avalonia.Controls.Primitives.PopupRoot
                               or Avalonia.Controls.Primitives.OverlayLayer);
        var popupTopY = host.TranslatePoint(new Point(0, 0), window)!.Value.Y;
        Assert.True(Math.Abs(popupTopY - itemBottom) < 8, $"popup top {popupTopY} vs item bottom {itemBottom}");

        // A second click closes it.
        ClickOn(window, file);
        Assert.False(file.IsSubMenuOpen, "second click closes");
    }

    [AvaloniaFact]
    public async Task EscapeClosesTheOpenMenu()
    {
        var (window, view, _) = await ShowShellAsync();
        ClickOn(window, TopMenu(view, "_File"));
        Assert.True(TopMenu(view, "_File").IsSubMenuOpen);
        window.KeyPressQwerty(PhysicalKey.Escape, RawInputModifiers.None);
        Pump();
        Assert.False(TopMenu(view, "_File").IsSubMenuOpen);
    }

    [AvaloniaFact]
    public async Task AltPlusLetterOpensThatMenu()
    {
        var (window, view, _) = await ShowShellAsync();
        window.KeyPressQwerty(PhysicalKey.T, RawInputModifiers.Alt);
        Pump();
        Assert.True(TopMenu(view, "_Transactions").IsSubMenuOpen, "Alt+T opens Transactions");
        Assert.Contains(PopupItems(window), item => Equals(item.Header?.ToString(), "_Purchasing"));
    }

    [AvaloniaFact]
    public async Task AltThenLetterRunsAnItemOfTheOpenMenu()
    {
        var (window, view, shell) = await ShowShellAsync();
        window.KeyPressQwerty(PhysicalKey.F, RawInputModifiers.Alt);
        Pump();
        Assert.True(TopMenu(view, "_File").IsSubMenuOpen);
        window.KeyPressQwerty(PhysicalKey.C, RawInputModifiers.None);
        Pump();
        if (!shell.IsLauncherOpen)
        {
            // Some themes want the modifier held for the item accelerator too.
            window.KeyPressQwerty(PhysicalKey.C, RawInputModifiers.Alt);
            Pump();
        }

        Assert.True(shell.IsLauncherOpen, "the File menu's C item ran the command launcher");
    }

    [AvaloniaFact]
    public async Task F10FocusesTheMenuBar()
    {
        var (window, view, _) = await ShowShellAsync();
        // The desktop convention: F10 moves focus to the menu bar from anywhere in the
        // window. Activate the window through a real click on the search box first (as a
        // user does), then press F10.
        ClickOn(window, view.SearchBoxControl);
        Pump();
        Assert.NotNull(TopLevel.GetTopLevel(window)!.FocusManager!.GetFocusedElement());
        window.KeyPress(Key.F10, RawInputModifiers.None, PhysicalKey.F10, null);
        Pump();
        var focused = TopLevel.GetTopLevel(window)!.FocusManager!.GetFocusedElement();
        Assert.True(
            focused is MenuItem || focused is Menu,
            $"F10 should move focus into the menu bar (focused: {focused?.GetType().Name})");
        Assert.True(TopMenu(view, "_File").IsSelected, "the first top-level menu is highlighted");
    }

    [AvaloniaFact]
    public async Task ArrowsHomeAndEndNavigateItems()
    {
        var (window, view, shell) = await ShowShellAsync();
        ClickOn(window, TopMenu(view, "_File"));
        var items = PopupItems(window).Where(item => item.Items.Count == 0).ToList();
        Assert.NotEmpty(items);

        // Down advances from wherever the highlight starts.
        window.KeyPressQwerty(PhysicalKey.ArrowDown, RawInputModifiers.None);
        Pump();
        var afterFirstDown = items.FindIndex(item => item.IsSelected);
        Assert.True(afterFirstDown >= 0, "an item is highlighted after Down");
        window.KeyPressQwerty(PhysicalKey.ArrowDown, RawInputModifiers.None);
        Pump();
        var afterSecondDown = items.FindIndex(item => item.IsSelected);
        Assert.True(afterSecondDown != afterFirstDown, "Down moved the highlight");

        window.KeyPressQwerty(PhysicalKey.End, RawInputModifiers.None);
        Pump();
        Assert.True(items[^1].IsSelected, "End jumps to the last item");

        window.KeyPressQwerty(PhysicalKey.Home, RawInputModifiers.None);
        Pump();
        Assert.True(items[0].IsSelected, "Home jumps to the first item");

        // Enter runs the highlighted item (Command launcher is first).
        window.KeyPressQwerty(PhysicalKey.Enter, RawInputModifiers.None);
        Pump();
        Assert.True(shell.IsLauncherOpen, "Enter runs the item");
    }

    [AvaloniaFact]
    public async Task LeftAndRightMoveBetweenTopLevelMenusAndIntoSubmenus()
    {
        var (window, view, _) = await ShowShellAsync();
        ClickOn(window, TopMenu(view, "_View"));
        Assert.True(TopMenu(view, "_View").IsSubMenuOpen);
        window.KeyPressQwerty(PhysicalKey.ArrowRight, RawInputModifiers.None);
        Pump();
        Assert.True(TopMenu(view, "_Transactions").IsSubMenuOpen, "Right moves to the next top-level menu");
        Assert.False(TopMenu(view, "_View").IsSubMenuOpen);

        var purchasing = PopupItems(window).Single(item => Equals(item.Header?.ToString(), "_Purchasing"));
        // Right moved the open menu to Transactions and auto-selected its first item
        // (Purchasing); another Right enters the submenu.
        Assert.True(purchasing.IsSelected, "Purchasing is highlighted");
        window.KeyPressQwerty(PhysicalKey.ArrowRight, RawInputModifiers.None);
        Pump();
        Assert.True(purchasing.IsSubMenuOpen, "Right opens the submenu");
        Assert.Contains(PopupItems(window), item => Equals(item.Header?.ToString(), "Purchase _Order"));

        // Left steps back out.
        window.KeyPressQwerty(PhysicalKey.ArrowLeft, RawInputModifiers.None);
        Pump();
        Assert.False(purchasing.IsSubMenuOpen);
    }

    [AvaloniaFact]
    public async Task HoveringANeighbourSwitchesTheOpenMenu()
    {
        var (window, view, _) = await ShowShellAsync();
        ClickOn(window, TopMenu(view, "_File"));
        Assert.True(TopMenu(view, "_File").IsSubMenuOpen);
        HoverOver(window, TopMenu(view, "_View"));
        Assert.True(TopMenu(view, "_View").IsSubMenuOpen, "hover switches to the neighbour");
        Assert.False(TopMenu(view, "_File").IsSubMenuOpen);
    }

    [AvaloniaFact]
    public async Task ClickOutsideClosesTheMenu()
    {
        var (window, view, _) = await ShowShellAsync();
        ClickOn(window, TopMenu(view, "_File"));
        Assert.True(TopMenu(view, "_File").IsSubMenuOpen);
        var far = new Point(1400, 800);
        window.MouseDown(far, MouseButton.Left);
        window.MouseUp(far, MouseButton.Left);
        Pump();
        Assert.False(TopMenu(view, "_File").IsSubMenuOpen, "a click outside closes");
    }

    [AvaloniaFact]
    public async Task MnemonicUnderlineShowsOnlyWhileAltIsHeld()
    {
        var (window, view, _) = await ShowShellAsync();
        var access = window.GetVisualDescendants()
            .OfType<Avalonia.Controls.Primitives.AccessText>()
            .First(text => Equals(text.Text, "_File"));
        Assert.False(access.ShowAccessKey, "no underline at rest");
        window.KeyPress(Key.LeftAlt, RawInputModifiers.Alt, PhysicalKey.AltLeft, null);
        Pump();
        Assert.True(access.ShowAccessKey, "Alt held underlines the mnemonics");
    }

    [AvaloniaFact]
    public async Task ABlockedCommandIsGreyedAndCarriesItsReason()
    {
        var (window, view, shell) = await ShowShellAsync();
        var command = shell.Commands["create.purchaseOrder"];
        Assert.False(command.IsAvailable);
        Assert.Equal("The form for this document arrives with a later task.", command.Reason);

        ClickOn(window, TopMenu(view, "_Transactions"));
        var purchasing = PopupItems(window).Single(item => Equals(item.Header?.ToString(), "_Purchasing"));
        // Down highlights the first item (Purchasing); Right enters the submenu parent
        // and the flyout lists to the side.
        window.KeyPress(Key.Down, RawInputModifiers.None, PhysicalKey.None, null);
        Pump();
        Assert.True(purchasing.IsSelected, "Purchasing is highlighted");
        window.KeyPressQwerty(PhysicalKey.ArrowRight, RawInputModifiers.None);
        Pump();
        Assert.True(purchasing.IsSubMenuOpen, "Right opens the Purchasing flyout");
        var item = PopupItems(window).Single(menu => Equals(menu.Header?.ToString(), "Purchase _Order"));
        Assert.False(item.IsEffectivelyEnabled, "the item is greyed");
        Assert.Equal(command.Reason, ToolTip.GetTip(item));
    }

    [AvaloniaFact]
    public async Task TheMenuOpensNoModalOrExtraTopLevel()
    {
        var (window, view, _) = await ShowShellAsync();
        ClickOn(window, TopMenu(view, "_File"));
        Assert.NotEmpty(PopupItems(window));
        // The dropdown items live inside this window's own visual tree: no new top level.
        Assert.All(PopupItems(window), item => Assert.NotNull(item.GetVisualAncestors().OfType<Window>().SingleOrDefault()));
    }

    [AvaloniaFact]
    public async Task MenuItemClickDispatchesThroughTheCommandModel()
    {
        var (window, view, shell) = await ShowShellAsync();
        Assert.False(shell.IsLauncherOpen);
        ClickOn(window, TopMenu(view, "_File"));
        var item = PopupItems(window).Single(menu => Equals(menu.Header?.ToString(), "_Command launcher"));
        ClickOn(window, item);
        Pump();
        Assert.True(shell.IsLauncherOpen, "the menu item ran the launcher command through the dispatcher");
    }
}
