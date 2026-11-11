using Avalonia.Headless.XUnit;
using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>The model's paging, ordering, filtering, expansion and breadcrumb contract — the
/// behaviour the control paints, exercised without a window.</summary>
public sealed class HierarchyModelTests
{
    private static HierarchyModel MakeModel(IHierarchySource source, int pageSize = 100)
    {
        var model = new HierarchyModel(source, pageSize);
        model.InitializeAsync(CancellationToken.None).GetAwaiter().GetResult();
        return model;
    }

    [Fact]
    public void OpenLoadsOnlyTheFirstRootPage()
    {
        var model = MakeModel(HierarchySamples.AccountsSource(10_000, branching: 250));

        Assert.True(model.IsInitialized);
        Assert.Equal(1, model.LoadedRootCount);
        Assert.Single(model.Rows);
        Assert.Equal(0, model.Rows[0].Depth);
    }

    [Fact]
    public async Task ExpandingLoadsOnePageAndTheViewportLoadsTheNext()
    {
        var source = HierarchySamples.AccountsSource(10_000, branching: 250);
        var model = MakeModel(source, pageSize: 100);
        var root = GraphFixtures.RootKey;

        await model.ExpandAsync(root, CancellationToken.None);
        Assert.True(model.IsExpanded(root));
        Assert.Equal(100, model.LoadedChildren(root).Count);
        Assert.Equal(101, model.RowCount); // the root plus its loaded page

        // Reaching the end of the loaded page asks for the next one.
        await model.LoadMoreForWindowAsync(model.RowCount - 1, 1, CancellationToken.None);
        Assert.Equal(200, model.LoadedChildren(root).Count);
    }

    [Fact]
    public async Task CollapseHidesChildrenAndPullsTheCursorUp()
    {
        var model = MakeModel(HierarchySamples.AccountsSource(10_000, branching: 8));
        var root = GraphFixtures.RootKey;
        await model.ExpandAsync(root, CancellationToken.None);
        var child = model.LoadedChildren(root)[3].Key;
        model.MoveCursorTo(child);
        Assert.Equal(4, model.CursorIndex);

        model.Collapse(root);

        Assert.False(model.IsExpanded(root));
        Assert.Single(model.Rows);
        Assert.Equal(root, model.Cursor);
    }

    [Fact]
    public async Task SortIsPushedToTheSourceAndReloads()
    {
        var source = HierarchySamples.AccountsSource(10_000, branching: 8);
        var model = MakeModel(source);
        var root = GraphFixtures.RootKey;
        await model.ExpandAsync(root, CancellationToken.None);
        var firstAscending = model.LoadedChildren(root)[0].Title;

        await model.SetQueryAsync(HierarchyQuery.Default with { Descending = true }, CancellationToken.None);
        await model.ExpandAsync(root, CancellationToken.None);

        var firstDescending = model.LoadedChildren(root)[0].Title;
        Assert.NotEqual(firstAscending, firstDescending);
        Assert.True(string.CompareOrdinal(firstDescending, firstAscending) > 0);
    }

    [Fact]
    public async Task FilterKeepsThePathToAMatchReachable()
    {
        var source = HierarchySamples.AccountsSource(10_000, branching: 250);
        var model = MakeModel(source);
        await model.SetQueryAsync(HierarchyQuery.Default.WithFilter("Account 000042"), CancellationToken.None);

        // The root survives as the ancestor of the match; expanding reveals only the match.
        Assert.Equal(1, model.RowCount);
        var root = model.Rows[0].Key;
        await model.ExpandAsync(root, CancellationToken.None);

        var child = Assert.Single(model.LoadedChildren(root));
        Assert.Equal("Account 000042", child.Title);
    }

    [Fact]
    public async Task BreadcrumbsFollowTheLoadedParentLinks()
    {
        var model = MakeModel(HierarchySamples.AccountsSource(10_000, branching: 8));
        var root = GraphFixtures.RootKey;
        await model.ExpandAsync(root, CancellationToken.None);
        var child = model.LoadedChildren(root)[2].Key;
        await model.ExpandAsync(child, CancellationToken.None);
        var grandchild = model.LoadedChildren(child)[1].Key;

        var trail = model.Breadcrumbs(grandchild);

        Assert.Equal(3, trail.Count);
        Assert.Equal(root, trail[0].Key);
        Assert.Equal(child, trail[1].Key);
        Assert.Equal(grandchild, trail[2].Key);
    }

    [Fact]
    public async Task MalformedCycleInTheDataTerminates()
    {
        var root = new GraphNodeKey("Account", "r");
        var a = new GraphNodeKey("Account", "a");
        var b = new GraphNodeKey("Account", "b");
        var c = new GraphNodeKey("Account", "c");
        var source = new InMemoryHierarchySource(
        [
            new HierarchyNode(root, null, "Root", "Account", true),
            new HierarchyNode(a, root, "A", "Account", true),
            new HierarchyNode(b, a, "B", "Account", true),
            new HierarchyNode(c, b, "C", "Account", true),
            new HierarchyNode(a, c, "A again", "Account", true),
        ]);
        var model = MakeModel(source);

        foreach (var key in new[] { root, a, b, c })
        {
            await model.ExpandAsync(key, CancellationToken.None);
        }

        var keys = model.Rows.Select(row => row.Key).ToList();
        Assert.Equal(keys.Count, keys.Distinct().Count());
        Assert.Equal(4, model.RowCount);
    }

    [Fact]
    public async Task LockedNodesKeepThePlaceholderAndNeverTheirTitle()
    {
        var source = GraphFixtures.OrganisationSource();
        var model = MakeModel(source);
        var head = model.Rows[0].Key;
        await model.ExpandAsync(head, CancellationToken.None);

        var locked = model.LoadedChildren(head).Single(node => node.IsLocked);
        Assert.Equal(HierarchyNode.LockedTitle, locked.DisplayTitle);
        Assert.Empty(locked.Title);
        Assert.True(locked.HasChildren);
    }
}
