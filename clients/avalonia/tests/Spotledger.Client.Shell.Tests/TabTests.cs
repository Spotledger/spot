using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The tab row as the user drives it: open workspaces from the rail, close the middle tab,
/// Ctrl+Tab cycling, Ctrl+W on the active tab. ViewModel level; the key bindings themselves
/// are exercised headless in <see cref="ShellViewKeyBindingTests"/>.
/// </summary>
public sealed class TabTests : IDisposable
{
    private static readonly string[] HomeSellingStock = ["Home", "Selling", "Stock"];
    private static readonly string[] HomeStock = ["Home", "Stock"];
    // WP-UI-02.T2: the sidebar carries the eight workspaces of the mockup.
    private static readonly string[] RailWorkspaces = ["Home", "My Tasks", "Selling", "Buying", "Stock", "Accounts", "Reports", "Settings"];

    private readonly ApiConnection _connection;
    private readonly ShellViewModel _shell;

    public TabTests()
    {
        // No fake response is ever consumed: OpenTabAsync is offline placeholder work.
        _connection = new ApiConnection(new FakeServerHandler());
        _shell = new ShellViewModel(_connection);
    }

    [Fact]
    public async Task OpenThreeTabsAndReopenFocusesTheExisting()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Stock", TestContext.Current.CancellationToken);

        Assert.Equal(3, _shell.Tabs.Count);
        Assert.Equal(HomeSellingStock, _shell.Tabs.Select(tab => tab.Workspace));
        Assert.Same(_shell.Tabs[2], _shell.SelectedTab);

        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);

        Assert.Equal(3, _shell.Tabs.Count);
        Assert.Same(_shell.Tabs[0], _shell.SelectedTab);
    }

    [Fact]
    public async Task CloseMiddleTabSelectsItsNeighbour()
    {
        await OpenThreeTabsAsync();

        var middle = _shell.Tabs[1];
        _shell.CloseTab(middle);

        Assert.Equal(HomeStock, _shell.Tabs.Select(tab => tab.Workspace));
        Assert.Same(_shell.Tabs[1], _shell.SelectedTab);
    }

    [Fact]
    public async Task CtrlTabCyclesForwardAndWraps()
    {
        await OpenThreeTabsAsync();
        _shell.SelectedTab = _shell.Tabs[0];

        _shell.SelectNextTabCommand.Execute(null);
        Assert.Equal("Selling", _shell.SelectedTab!.Workspace);

        _shell.SelectNextTabCommand.Execute(null);
        Assert.Equal("Stock", _shell.SelectedTab.Workspace);

        _shell.SelectNextTabCommand.Execute(null);
        Assert.Equal("Home", _shell.SelectedTab.Workspace);
    }

    [Fact]
    public async Task CtrlWClosesTheActiveTab()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        _shell.SelectedTab = _shell.Tabs[1];

        _shell.CloseActiveTabCommand.Execute(null);

        Assert.Single(_shell.Tabs);
        Assert.Equal("Home", _shell.SelectedTab!.Workspace);
    }

    [Fact]
    public void ClosingTheLastTabClearsSelection()
    {
        var tab = new WorkspaceTabViewModel("Home");
        _shell.Tabs.Add(tab);
        _shell.SelectedTab = tab;

        _shell.CloseActiveTabCommand.Execute(null);

        Assert.Empty(_shell.Tabs);
        Assert.Null(_shell.SelectedTab);
    }

    [Fact]
    public void RailHoldsThePlaceholderWorkspaces()
    {
        Assert.Equal(RailWorkspaces, ShellViewModel.Workspaces);
    }

    [Fact]
    public void RightToLeftSwitchesTheFlowDirection()
    {
        Assert.Equal(Avalonia.Media.FlowDirection.LeftToRight, _shell.FlowDirection);

        _shell.RightToLeft = true;

        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, _shell.FlowDirection);
    }

    private async Task OpenThreeTabsAsync()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Stock", TestContext.Current.CancellationToken);
    }

    public void Dispose() => _connection.Dispose();
}
