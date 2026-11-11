using Spotledger.Demo.Mock;
using Xunit;

namespace Spotledger.Demo.Tests;

public sealed class StateTests
{
    private static async Task<ShellViewModel> CreateAsync()
    {
        var model = new ShellViewModel(new MockBackend()); await model.InitializeAsync(); return model;
    }
    [Fact]
    public async Task UuidIdentityAndPresentationStateSurviveSwitches()
    {
        var model = await CreateAsync(); var record = model.Home.Records[0];
        model.OpenRecord(record); var tab = model.Tabs.Active!;
        tab.Edit("Unsaved"); tab.Filter = "needle"; tab.ScrollOffset = 123;
        await model.RunAsync("workspace.reports", CommandRoute.Menu);
        model.OpenRecord(record with { Number = "Changed display number" });
        Assert.Same(tab, model.Tabs.Active); Assert.Equal(3, model.Tabs.Tabs.Count);
        Assert.Equal("Unsaved", tab.Note); Assert.Equal("needle", tab.Filter); Assert.Equal(123, tab.ScrollOffset);
        model.Tabs.Cycle(1); Assert.Equal("workspace.reports", model.Tabs.Active!.Key);
        model.Tabs.Cycle(-1); Assert.Same(tab, model.Tabs.Active);
    }
    [Theory]
    [InlineData(DirtyChoice.Save)]
    [InlineData(DirtyChoice.Discard)]
    [InlineData(DirtyChoice.Cancel)]
    public async Task DirtyCloseHandlesAllThreeChoices(DirtyChoice choice)
    {
        var model = await CreateAsync(); model.OpenRecord(model.Home.Records[0]);
        var tab = model.Tabs.Active!; tab.Edit("Changed"); var prompts = 0;
        model.AskDirtyAsync = selected => { Assert.Same(tab, selected); prompts++; return Task.FromResult(choice); };
        var closed = await model.CloseAsync([tab]); Assert.Equal(1, prompts);
        Assert.Equal(choice != DirtyChoice.Cancel, closed);
        Assert.Equal(choice == DirtyChoice.Cancel, model.Tabs.Tabs.Contains(tab));
        var home = (await model.Backend.GetHomeAsync()).Value!;
        Assert.Equal(choice == DirtyChoice.Save ? "Changed" : "Seed note", home.Records[0].Note);
    }
    [Fact]
    public async Task BulkClosePreservesPinsAndCancelKeepsAllTabs()
    {
        var model = await CreateAsync(); model.Tabs.Active!.IsPinned = true;
        model.OpenRecord(model.Home.Records[0]); var first = model.Tabs.Active!;
        model.OpenRecord(model.Home.Records[1]); var second = model.Tabs.Active!; second.Edit("dirty");
        await model.RunAsync("workspace.reports", CommandRoute.Menu); var report = model.Tabs.Active!;
        model.SelectTab(first);
        Assert.Equal(new[] { second, report }, model.Tabs.RightOf(first));
        Assert.Equal(new[] { second, report }, model.Tabs.Others(first));
        Assert.False(await model.CloseAsync(model.Tabs.RightOf(first))); Assert.Equal(4, model.Tabs.Tabs.Count);
        model.AskDirtyAsync = _ => Task.FromResult(DirtyChoice.Discard);
        Assert.True(await model.CloseAsync(model.Tabs.RightOf(first))); Assert.Equal(2, model.Tabs.Tabs.Count);
        model.Tabs.Active!.IsPinned = true;
        Assert.True(await model.CloseAsync([first])); // Explicit close may close a pinned tab.
        Assert.Single(model.Tabs.Tabs);
    }
    [Fact]
    public async Task RoleSwitchRequiresDirtyDecisionAndChangesHome()
    {
        var model = await CreateAsync(); var initial = model.Home;
        model.OpenRecord(initial.Records[0]); var tab = model.Tabs.Active!; tab.Edit("keep");
        Assert.False(await model.SwitchRoleAsync(DemoRole.Cashier)); Assert.Same(initial, model.Home); Assert.True(tab.IsDirty);
        model.AskDirtyAsync = _ => Task.FromResult(DirtyChoice.Save);
        Assert.True(await model.SwitchRoleAsync(DemoRole.Cashier));
        Assert.Equal(DemoRole.Cashier, model.Home.User.Role); Assert.Contains("workspace.pos", model.Home.Shortcuts);
        Assert.NotEqual(initial.Tasks[0].Title, model.Home.Tasks[0].Title); Assert.Equal("keep", tab.Record!.Note);
        await model.SwitchRoleAsync(DemoRole.Manager); Assert.Empty(model.Home.Tasks);
    }
    [Fact]
    public async Task LayoutIsPerUserAndResetClosesTabsAndRestoresSeed()
    {
        var model = await CreateAsync();
        await model.RunAsync("view.sidebar", CommandRoute.Shortcut); await model.RunAsync("view.rail", CommandRoute.Menu);
        await model.SwitchRoleAsync(DemoRole.Cashier);
        Assert.Equal(SidebarMode.Expanded, model.Sidebar); Assert.True(model.RailVisible);
        await model.SwitchRoleAsync(DemoRole.ProcurementOfficer);
        Assert.Equal(SidebarMode.Icons, model.Sidebar); Assert.False(model.RailVisible);
        model.OpenRecord(model.Home.Records[0]); model.Tabs.Active!.Edit("Changed");
        Assert.False(await model.ResetAsync()); Assert.Equal(2, model.Tabs.Tabs.Count);
        model.AskDirtyAsync = _ => Task.FromResult(DirtyChoice.Discard);
        Assert.True(await model.ResetAsync()); Assert.Single(model.Tabs.Tabs);
        Assert.Equal("home", model.Tabs.Active!.Key); Assert.Equal("Seed note", model.Home.Records[0].Note);
        Assert.Equal(SidebarMode.Expanded, model.Sidebar); Assert.True(model.RailVisible);
    }
    [Theory]
    [InlineData("desktop")]
    [InlineData("browser")]
    public async Task KeymapAndMnemonicsHaveNoCollisions(string name)
    {
        var model = await CreateAsync(); var profile = KeymapProfile.Load(name); profile.Validate(model.Registry);
        var duplicate = profile with { Bindings = profile.Bindings.Append(profile.Bindings[0]).ToArray() };
        Assert.Throws<InvalidOperationException>(() => duplicate.Validate(model.Registry));
        model.Registry.Add(new("collision", "Duplicate", "Test", "Open", [], "File", 'O'));
        Assert.Throws<InvalidOperationException>(() => profile.Validate(model.Registry));
        if (name == "browser") Assert.False(profile.AltMenus);
    }
    [Fact]
    public void KeyboardDocumentationMatchesGeneratedProfiles()
    {
        var directory = new DirectoryInfo(AppContext.BaseDirectory);
        while (directory is not null && !File.Exists(Path.Combine(directory.FullName, "docs", "KeyboardMap.md"))) directory = directory.Parent;
        Assert.NotNull(directory);
        var file = Path.Combine(directory.FullName, "docs", "KeyboardMap.md");
        Assert.Equal(KeymapProfile.GenerateMarkdown().Replace("\r\n", "\n", StringComparison.Ordinal),
            File.ReadAllText(file).Replace("\r\n", "\n", StringComparison.Ordinal));
    }
}
