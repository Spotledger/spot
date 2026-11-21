using System.Diagnostics;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media;
using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>The keyboard model through the real OnKeyDown path: arrows, Home/End, expand and
/// collapse, <c>*</c>, type-ahead, selection and activation.</summary>
public sealed class HierarchyKeyboardTests
{
    private static SpotHierarchy MakeTree(int branching = 8)
        => GraphFixtures.MakeHierarchy(HierarchySamples.AccountsSource(10_000, branching));

    [AvaloniaFact]
    public void ArrowsMoveTheCursorAndSelectionFollows()
    {
        var tree = MakeTree();
        var root = GraphFixtures.RootKey;
        GraphFixtures.Press(tree, Key.Right); // expand
        TestHost.Pump();

        GraphFixtures.Press(tree, Key.Down);

        Assert.Equal(1, tree.Model!.CursorIndex);
        Assert.Equal(tree.Model.Rows[1].Key, tree.Model.Cursor);
        Assert.True(tree.Model.Selection.Contains(tree.Model.Cursor!.Value));
        Assert.True(tree.Model.IsExpanded(root));
    }

    [AvaloniaFact]
    public void RightExpandsThenEntersTheChildrenAndLeftComesBack()
    {
        var tree = MakeTree();
        var model = tree.Model!;
        var root = GraphFixtures.RootKey;

        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        Assert.True(model.IsExpanded(root));
        Assert.Equal(0, model.CursorIndex);

        GraphFixtures.Press(tree, Key.Right);
        Assert.Equal(1, model.CursorIndex);

        GraphFixtures.Press(tree, Key.Left); // a leaf: back to the parent
        Assert.Equal(0, model.CursorIndex);

        GraphFixtures.Press(tree, Key.Left); // expanded: collapse
        Assert.False(model.IsExpanded(root));
        Assert.Single(model.Rows);
    }

    [AvaloniaFact]
    public void HomeAndEndJumpToTheVisibleEdges()
    {
        var tree = MakeTree();
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        GraphFixtures.Press(tree, Key.End);
        Assert.Equal(model.RowCount - 1, model.CursorIndex);

        GraphFixtures.Press(tree, Key.Home);
        Assert.Equal(0, model.CursorIndex);
    }

    [AvaloniaFact]
    public void PageDownMovesByOneViewport()
    {
        var tree = MakeTree(branching: 250);
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        GraphFixtures.Press(tree, Key.PageDown);

        Assert.True(model.CursorIndex > 1);
        Assert.Equal(Math.Min(model.RowCount - 1, tree.VisibleRowCount()), model.CursorIndex);
    }

    [AvaloniaFact]
    public void StarExpandsEverySibling()
    {
        var tree = MakeTree();
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();
        GraphFixtures.Press(tree, Key.Down); // cursor on a child that has children
        var child = model.Cursor!.Value;
        Assert.True(model.FindNode(child)!.HasChildren);

        GraphFixtures.Press(tree, Key.Multiply);
        TestHost.Pump();

        foreach (var sibling in model.LoadedChildren(GraphFixtures.RootKey))
        {
            if (sibling.HasChildren)
            {
                Assert.True(model.IsExpanded(sibling.Key), "every sibling with children must expand");
            }
        }
    }

    [AvaloniaFact]
    public void TypeAheadJumpsToAMatchingTitle()
    {
        var tree = MakeTree(branching: 250);
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        GraphFixtures.Type(tree, "Account 000150");

        Assert.Equal("Account 000150", model.Rows[model.CursorIndex].Title);
    }

    [AvaloniaFact]
    public void TypeAheadBufferResetsAfterTheWindow()
    {
        var tree = MakeTree(branching: 250);
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        GraphFixtures.Type(tree, "Account 0001");
        var clock = (FixedClock)tree.Clock;
        clock.Now = clock.Now.AddSeconds(2);
        GraphFixtures.Type(tree, "5");

        Assert.Equal("5", model.TypeAheadBuffer);
    }

    [AvaloniaFact]
    public void ShiftExtendsTheSelectionAndControlASelectsEveryVisibleRow()
    {
        var tree = MakeTree();
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        GraphFixtures.Press(tree, Key.Down);
        GraphFixtures.Press(tree, Key.Down, KeyModifiers.Shift);
        GraphFixtures.Press(tree, Key.Down, KeyModifiers.Shift);
        Assert.Equal(3, model.Selection.Count);

        GraphFixtures.Press(tree, Key.A, KeyModifiers.Control);
        Assert.Equal(model.RowCount, model.Selection.Count);
    }

    [AvaloniaFact]
    public void SpaceTogglesAndEnterOpensThroughTheNavigationSeam()
    {
        var tree = MakeTree();
        var navigation = new RecordingNavigation();
        tree.Navigation = navigation;
        var activated = new List<GraphNodeKey>();
        tree.RowActivated += (_, e) => activated.Add(e.Key);

        GraphFixtures.Press(tree, Key.Down);
        Assert.Equal(1, tree.Model!.Selection.Count);
        GraphFixtures.Press(tree, Key.Space);
        Assert.Equal(0, tree.Model.Selection.Count);
        GraphFixtures.Press(tree, Key.Space);
        Assert.Equal(1, tree.Model.Selection.Count);

        var cursor = tree.Model.Cursor!.Value;
        GraphFixtures.Press(tree, Key.Enter);

        Assert.Equal([cursor], activated);
        Assert.Equal(cursor, Assert.Single(navigation.Opened));
    }

    [AvaloniaFact]
    public void RightToLeftSwapsTheExpandAndCollapseKeys()
    {
        var tree = MakeTree();
        tree.FlowDirection = FlowDirection.RightToLeft;
        var model = tree.Model!;
        var root = GraphFixtures.RootKey;

        GraphFixtures.Press(tree, Key.Left);
        TestHost.Pump();
        Assert.True(model.IsExpanded(root));

        GraphFixtures.Press(tree, Key.Right);
        Assert.False(model.IsExpanded(root));
    }

    [AvaloniaFact]
    public void KeyboardNavigationStaysWithinTheFiftyMillisecondBudget()
    {
        var tree = MakeTree(branching: 250);
        var model = tree.Model!;
        GraphFixtures.Press(tree, Key.Right);
        TestHost.Pump();

        var samples = new List<double>();
        for (var i = 0; i < 200; i++)
        {
            var stopwatch = Stopwatch.StartNew();
            GraphFixtures.Press(tree, i % 2 == 0 ? Key.Down : Key.Up);
            stopwatch.Stop();
            samples.Add(stopwatch.Elapsed.TotalMilliseconds);
        }

        // Each key press is a paint of one viewport at most; the p95 must stay well under the
        // 50 ms keyboard budget even on a shared CI box.
        samples.Sort();
        var p95 = samples[(int)Math.Ceiling(samples.Count * 0.95) - 1];
        Assert.True(p95 < 50, $"keyboard p95 {p95:F2} ms");
        Assert.True(model.RowCount >= 2);
    }
}
