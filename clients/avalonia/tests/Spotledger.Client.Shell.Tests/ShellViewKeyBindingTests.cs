using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// Headless view test for the shell's tab keys: Ctrl+Tab cycles the selected tab and
/// Ctrl+W closes the active one, driven through the real Avalonia input pipeline against
/// <see cref="ShellView"/>.
/// </summary>
public sealed class ShellViewKeyBindingTests
{
    private static readonly string[] HomeSelling = ["Home", "Selling"];

    [AvaloniaFact]
    public async Task CtrlTabCyclesAndCtrlWClosesTheActiveTab()
    {
        using var connection = new ApiConnection(new FakeServerHandler());
        var shell = new ShellViewModel(connection);
        await shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        await shell.OpenTabAsync("Stock", TestContext.Current.CancellationToken);
        shell.SelectedTab = shell.Tabs[0];

        var view = new ShellView { DataContext = shell };
        var window = new Window
        {
            Content = view,
        };
        window.Show();

        // Focus the view itself so the key events route through ShellView.OnKeyDown.
        view.Focusable = true;
        view.Focus();

        window.KeyPressQwerty(PhysicalKey.Tab, RawInputModifiers.Control);
        Assert.Equal("Selling", shell.SelectedTab!.Workspace);

        window.KeyPressQwerty(PhysicalKey.Tab, RawInputModifiers.Control);
        Assert.Equal("Stock", shell.SelectedTab.Workspace);

        // Ctrl+W closes the active tab; the neighbour takes the selection.
        window.KeyPressQwerty(PhysicalKey.W, RawInputModifiers.Control);
        Assert.Equal(HomeSelling, shell.Tabs.Select(tab => tab.Workspace));
        Assert.Equal("Selling", shell.SelectedTab!.Workspace);
    }
}
