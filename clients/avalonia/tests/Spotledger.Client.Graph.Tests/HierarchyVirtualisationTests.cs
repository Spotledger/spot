using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>Virtualisation: a 10,000-node tree paints the viewport, not the tree; scrolling
/// to the end of a loaded page asks for the next one; right-to-left flow mirrors the
/// indentation. This is the acceptance the brief states (navigable within the budget).</summary>
public sealed class HierarchyVirtualisationTests
{
    private static void Render(Control control)
    {
        Dispatcher.UIThread.RunJobs();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        Dispatcher.UIThread.RunJobs();
        control.InvalidateVisual();
        AvaloniaHeadlessPlatform.ForceRenderTimerTick();
        Dispatcher.UIThread.RunJobs();
    }

    [AvaloniaFact]
    public void OnlyTheViewportRowsArePainted()
    {
        var tree = GraphFixtures.MakeHierarchy(
            HierarchySamples.AccountsSource(10_000, branching: 250),
            pageSize: 100,
            height: 300);
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        Render(tree);

        var visible = tree.VisibleRowCount();
        Assert.True(visible > 2);
        Assert.InRange(tree.LastRenderedRowCount, 1, visible);
        Assert.True(tree.Model!.RowCount > tree.LastRenderedRowCount, "the tree is larger than the paint");
    }

    [AvaloniaFact]
    public void ScrollingToTheEndOfALoadedPageLoadsTheNextPage()
    {
        var tree = GraphFixtures.MakeHierarchy(
            HierarchySamples.AccountsSource(10_000, branching: 250),
            pageSize: 100,
            height: 300);
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        Assert.Equal(100, tree.Model!.LoadedChildren(GraphFixtures.RootKey).Count);

        tree.ScrollToRow(tree.MaxScrollRows);
        TestHost.Pump();
        TestHost.Pump();

        Assert.Equal(200, tree.Model.LoadedChildren(GraphFixtures.RootKey).Count);
        Render(tree);
        Assert.True(tree.LastRenderedRowCount <= tree.VisibleRowCount());
    }

    [AvaloniaFact]
    public void ScrollingAThousandRowsKeepsThePaintBounded()
    {
        var tree = GraphFixtures.MakeHierarchy(
            HierarchySamples.AccountsSource(10_000, branching: 250),
            pageSize: 100,
            height: 300);
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        for (var step = 0; step < 20; step++)
        {
            tree.ScrollBy(10);
            TestHost.Pump();
        }

        Render(tree);
        Assert.True(tree.ScrollRows > 100);
        Assert.True(tree.LastRenderedRowCount <= tree.VisibleRowCount());
    }

    [AvaloniaFact]
    public void RightToLeftMirrorsIndentationAndChevrons()
    {
        var leftToRight = GraphFixtures.MakeHierarchy(GraphFixtures.OrganisationSource(), width: 500);
        var rightToLeft = GraphFixtures.MakeHierarchy(GraphFixtures.OrganisationSource(), width: 500);
        rightToLeft.FlowDirection = FlowDirection.RightToLeft;

        Assert.False(leftToRight.IsRtl);
        Assert.True(rightToLeft.IsRtl);

        // Right-to-left mirrors the run: the chevron starts at the right edge and each level
        // indents further left, so RTL level 0 sits to the right of LTR level 0 and RTL
        // level 2 sits left of RTL level 0.
        Assert.True(rightToLeft.ChevronLeft(0) > leftToRight.ChevronLeft(0));
        Assert.True(rightToLeft.ChevronLeft(2) < rightToLeft.ChevronLeft(0));
        Assert.Equal(6 + (2 * 20), leftToRight.ChevronLeft(2));
    }

    [AvaloniaFact]
    public void BreadcrumbBandFollowsTheCursorPath()
    {
        var tree = GraphFixtures.MakeHierarchy(GraphFixtures.OrganisationSource());
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        GraphFixtures.Press(tree, Key.Down);
        GraphFixtures.Press(tree, Key.Down);
        Render(tree);

        var trail = model.Breadcrumbs(model.Cursor);
        Assert.Equal(2, trail.Count);
        Assert.Equal("Spotledger Holdings", trail[0].Title);
        Assert.True(tree.ShowBreadcrumb);
    }

    [AvaloniaFact]
    public void LockedRowsPaintThePlaceholderNotTheHiddenTitle()
    {
        var tree = GraphFixtures.MakeHierarchy(GraphFixtures.OrganisationSource());
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        Render(tree);

        Assert.Contains(HierarchyNode.LockedTitle, tree.LastRenderedTitles);
        Assert.DoesNotContain("Department hidden", tree.LastRenderedTitles);
    }
}
