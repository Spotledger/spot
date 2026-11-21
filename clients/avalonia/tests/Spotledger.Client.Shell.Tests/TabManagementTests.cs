using Spotledger.Client.Api;
using Spotledger.Client.Shell;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The tab rules of the brief: identity is the document UUID (ADR-33), opening an open
/// record focuses its tab; a dirty close asks Save/Discard/Cancel; closing many gathers
/// every decision first and one Cancel keeps all tabs; Close Others spares pinned tabs.
/// </summary>
public sealed class TabManagementTests : IDisposable
{
    private readonly ApiConnection _connection;
    private readonly ShellViewModel _shell;

    public TabManagementTests()
    {
        _connection = new ApiConnection(new FakeServerHandler());
        _shell = new ShellViewModel(_connection);
    }

    public void Dispose() => _connection.Dispose();

    [Fact]
    public async Task OpeningAnOpenRecordFocusesItsTabInsteadOfDuplicating()
    {
        var id = Guid.NewGuid();
        var first = _shell.OpenDocumentTab("Buying", id, "Purchase Order", "PO-00231");
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        var again = _shell.OpenDocumentTab("Buying", id, "Purchase Order", "PO-00231");

        Assert.Same(first, again);
        Assert.Equal(2, _shell.Tabs.Count);
        Assert.Same(first, _shell.SelectedTab);
    }

    [Fact]
    public void TabIdentityIsTheUuidNotTheDocumentNumber()
    {
        var a = _shell.OpenDocumentTab("Buying", Guid.NewGuid(), "Purchase Order", "PO-00231");
        var b = _shell.OpenDocumentTab("Buying", Guid.NewGuid(), "Purchase Order", "PO-00231");
        Assert.NotSame(a, b);
        Assert.Equal(2, _shell.Tabs.Count);
        Assert.NotEqual(a.Key, b.Key);
        Assert.StartsWith("document:", a.Key, StringComparison.Ordinal);
    }

    [Fact]
    public async Task WorkspaceTabsKeepTheirStableKey()
    {
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        var tab = _shell.Tabs.Single();
        Assert.Equal("workspace:Selling", tab.Key);
        Assert.Equal("Selling", tab.Workspace);
        Assert.Null(tab.DocumentId);
    }

    [Fact]
    public async Task DirtyCloseAsksAndSaveKeepsTheClose()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        var tab = _shell.Tabs.Single();
        tab.IsDirty = true;
        var asked = 0;
        _shell.ClosePromptRequested = prompted =>
        {
            asked++;
            Assert.Same(tab, prompted);
            return Task.FromResult(TabCloseChoice.Save);
        };

        await _shell.RunAsync("tab.close", CommandRoute.Menu);

        Assert.Equal(1, asked);
        Assert.Empty(_shell.Tabs);
    }

    [Fact]
    public async Task DirtyCloseCancelKeepsTheTab()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        _shell.Tabs.Single().IsDirty = true;
        _shell.ClosePromptRequested = _ => Task.FromResult(TabCloseChoice.Cancel);

        await _shell.RunAsync("tab.close", CommandRoute.Menu);

        Assert.Single(_shell.Tabs);
    }

    [Fact]
    public async Task DirtyCloseDiscardClosesWithoutSaving()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        var tab = _shell.Tabs.Single();
        tab.IsDirty = true;
        _shell.ClosePromptRequested = _ => Task.FromResult(TabCloseChoice.Discard);

        await _shell.RunAsync("tab.close", CommandRoute.Menu);

        Assert.Empty(_shell.Tabs);
    }

    [Fact]
    public async Task ClosingManyGathersEveryDecisionFirstAndCancelKeepsAllTabs()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Stock", TestContext.Current.CancellationToken);
        _shell.Tabs[0].IsDirty = true;
        _shell.Tabs[1].IsDirty = true;
        var questions = new List<string>();
        _shell.ClosePromptRequested = tab =>
        {
            questions.Add(tab.Workspace);
            // Answer Home with Discard, then Cancel on Selling: no tab may close yet.
            return Task.FromResult(tab.Workspace == "Home" ? TabCloseChoice.Discard : TabCloseChoice.Cancel);
        };

        await _shell.RunAsync("tab.all", CommandRoute.Menu);

        Assert.Equal(["Home", "Selling"], questions);
        Assert.Equal(3, _shell.Tabs.Count);
    }

    [Fact]
    public async Task CloseOthersKeepsActiveAndPinnedTabs()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Stock", TestContext.Current.CancellationToken);
        _shell.SelectedTab = _shell.Tabs[1];
        _shell.Tabs[0].IsPinned = true;

        await _shell.RunAsync("tab.others", CommandRoute.ContextMenu);

        // Close Others keeps the active tab and every pinned tab (brief: "Keep this tab
        // and pinned tabs").
        Assert.Equal(["Home", "Selling"], _shell.Tabs.Select(tab => tab.Workspace));
        Assert.Same(_shell.Tabs[1], _shell.SelectedTab);
    }

    [Fact]
    public async Task PinAndUnpinThroughTheCommandModel()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        var tab = _shell.Tabs.Single();
        await _shell.RunAsync("tab.pin", CommandRoute.Menu);
        Assert.True(tab.IsPinned);
        await _shell.RunAsync("tab.pin", CommandRoute.ContextMenu);
        Assert.False(tab.IsPinned);
    }

    [Fact]
    public async Task DuplicateOpensASecondTabOfTheSameDocument()
    {
        var id = Guid.NewGuid();
        _shell.OpenDocumentTab("Buying", id, "Purchase Order", "PO-00231");
        var before = _shell.Tabs.Count;

        await _shell.RunAsync("tab.duplicate", CommandRoute.ContextMenu);

        Assert.Equal(before + 1, _shell.Tabs.Count);
        Assert.Equal(_shell.Tabs[^1], _shell.SelectedTab);
    }

    [Fact]
    public async Task MoveToNewWindowIsBlockedWithAReasonAndNothingIsInert()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        var result = await _shell.RunAsync("tab.newWindow", CommandRoute.ContextMenu);
        Assert.False(result.Executed);
        Assert.Contains("Multi-window", result.Availability.Reason);
        Assert.Equal("Multi-window support arrives with a later task.", _shell.StatusMessage);
    }

    [Fact]
    public async Task CtrlChordsFromTheKeymapDriveTheSameCommands()
    {
        await _shell.OpenTabAsync("Home", TestContext.Current.CancellationToken);
        await _shell.OpenTabAsync("Selling", TestContext.Current.CancellationToken);
        _shell.SelectedTab = _shell.Tabs[0];

        var desktop = KeymapProfile.Load("desktop");
        var closeChord = desktop.Find("Ctrl+W")!;
        Assert.Equal("tab.close", closeChord.CommandId);
        await _shell.RunAsync(closeChord.CommandId, CommandRoute.Shortcut);
        Assert.Equal("Selling", _shell.SelectedTab!.Workspace);
    }
}
