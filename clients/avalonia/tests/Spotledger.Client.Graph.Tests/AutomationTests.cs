using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>Accessibility: both views expose named, typed peers with stable key-derived
/// automation ids; the tree's row peers carry the expand/collapse and selection patterns; a
/// locked row or card names only the placeholder.</summary>
public sealed class AutomationTests
{
    private static SpotHierarchy MakeTree()
    {
        var tree = GraphFixtures.MakeHierarchy(GraphFixtures.OrganisationSource(), height: 400);
        TestHost.Await(tree.Model!.ExpandAsync(GraphFixtures.OrganisationRoot, CancellationToken.None));
        TestHost.Pump();
        tree.InvalidateVisual();
        TestHost.Pump();
        return tree;
    }

    [AvaloniaFact]
    public void TheTreePeerIsATreeWithAStableAutomationId()
    {
        var tree = MakeTree();
        var peer = Assert.IsType<SpotHierarchyAutomationPeer>(tree.OnCreateAutomationPeerShim());

        Assert.Equal(AutomationControlType.Tree, peer.GetAutomationControlType());
        Assert.Equal("Spotledger hierarchy", peer.GetName());
        Assert.Equal("graph-hierarchy", peer.GetAutomationId());
        Assert.NotEmpty(peer.GetChildren()!);
    }

    [AvaloniaFact]
    public void RowPeersCarryTheirLevelAndState()
    {
        var tree = MakeTree();
        var peer = tree.OnCreateAutomationPeerShim();
        var rows = peer.GetChildren()!;
        var root = Assert.IsType<SpotHierarchyRowAutomationPeer>(rows[0]);

        Assert.Equal(AutomationControlType.TreeItem, root.GetAutomationControlType());
        Assert.Equal("Spotledger Holdings, level 1, expanded", root.GetName());
        Assert.Equal(ExpandCollapseState.Expanded, root.ExpandCollapseState);
        Assert.StartsWith("graph-hierarchy-row-", root.GetAutomationId(), StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void TheExpandPatternExpandsAndCollapses()
    {
        var tree = MakeTree();
        var model = tree.Model!;
        var rootKey = GraphFixtures.OrganisationRoot;

        var root = Assert.IsType<SpotHierarchyRowAutomationPeer>(tree.OnCreateAutomationPeerShim().GetChildren()![0]);
        Assert.Equal(ExpandCollapseState.Expanded, root.ExpandCollapseState);

        root.Collapse();
        Assert.False(model.IsExpanded(rootKey));
        Assert.Equal(
            ExpandCollapseState.Collapsed,
            new SpotHierarchyRowAutomationPeer(tree, model.Rows[0]).ExpandCollapseState);

        root.Expand();
        TestHost.Pump();

        Assert.True(model.IsExpanded(rootKey));
        Assert.Equal(ExpandCollapseState.Expanded, root.ExpandCollapseState);
    }

    [AvaloniaFact]
    public void TheSelectionPatternSelectsAndReportsSelection()
    {
        var tree = MakeTree();
        var model = tree.Model!;
        var peer = Assert.IsType<SpotHierarchyAutomationPeer>(tree.OnCreateAutomationPeerShim());
        var selection = Assert.IsAssignableFrom<ISelectionProvider>(peer);
        var second = Assert.IsType<SpotHierarchyRowAutomationPeer>(peer.GetChildren()![1]);

        second.Select();

        Assert.True(second.IsSelected);
        Assert.Contains(model.Rows[1].Key, model.Selection.Keys);
        Assert.NotEmpty(selection.GetSelection());
    }

    [AvaloniaFact]
    public void ALockedRowNamesOnlyThePlaceholder()
    {
        var tree = MakeTree();
        var peer = tree.OnCreateAutomationPeerShim();
        var names = peer.GetChildren()!.Select(child => child.GetName()).ToList();

        Assert.Contains("Locked item, level 2", names);
        Assert.DoesNotContain(names, name => name?.Contains("hidden", StringComparison.OrdinalIgnoreCase) == true);
    }

    [AvaloniaFact]
    public void WholeSetModeExposesRowsOutsideTheViewport()
    {
        var tree = GraphFixtures.MakeHierarchy(HierarchySamples.AccountsSource(1_000, branching: 250), height: 120);
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        SpotHierarchy.AutomationForWholeSet = true;
        try
        {
            var peer = tree.OnCreateAutomationPeerShim();
            Assert.Equal(tree.Model!.RowCount, peer.GetChildren()!.Count);
        }
        finally
        {
            SpotHierarchy.AutomationForWholeSet = false;
        }
    }

    [AvaloniaFact]
    public void TheLineagePeerIsAPaneOfInvocableCards()
    {
        var view = new SpotLineageView { Clock = new FixedClock() };
        var navigation = new RecordingNavigation();
        view.Navigation = navigation;
        TestHost.Show(view, 800, 600);
        view.Snapshot = GraphFixtures.SupplyChain();
        TestHost.Await(view.PendingLayout);
        TestHost.Pump();

        var peer = Assert.IsType<SpotLineageAutomationPeer>(view.OnCreateAutomationPeerShim());
        Assert.Equal(AutomationControlType.Pane, peer.GetAutomationControlType());
        Assert.Equal("graph-lineage", peer.GetAutomationId());
        var cards = peer.GetChildren()!;
        Assert.Equal(view.Layout!.Nodes.Count, cards.Count);

        var invoice = cards.Single(card => card.GetName()?.StartsWith("Invoice 1, Draft", StringComparison.Ordinal) == true);
        Assert.StartsWith("graph-lineage-node-", invoice.GetAutomationId(), StringComparison.Ordinal);
        Assert.IsAssignableFrom<IInvokeProvider>(invoice).Invoke();

        Assert.Equal(new GraphNodeKey("SalesInvoice", "SI-1"), Assert.Single(navigation.Opened));

        var locked = cards.Single(card => card.GetName() == LineageNode.LockedTitle);
        Assert.NotNull(locked.GetAutomationId());
    }
}
