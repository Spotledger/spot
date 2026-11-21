using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Graph;
using Xunit;

namespace Spotledger.Client.Graph.Tests;

/// <summary>Re-parenting: the server answers validity, an applied move is a document update
/// that registers its inverse on the workspace undo stack, a refusal never touches the
/// document, and the client never decides a cycle itself.</summary>
public sealed class ReparentTests
{
    private static readonly GraphNodeKey Node = new("Account", "a");
    private static readonly GraphNodeKey Target = new("Account", "b");

    [Fact]
    public async Task HoverAsksTheServerAndDropAppliesTheMove()
    {
        var editor = new FakeHierarchyEditor();
        var undo = new RecordingUndo();
        var controller = new HierarchyReparentController(editor, undo);

        controller.BeginDrag(Node);
        controller.Hover(Target);
        var decision = await controller.ResolveHoverAsync(CancellationToken.None);
        Assert.True(decision!.Allowed);
        Assert.Equal(Target, controller.HoverTarget);

        var outcome = await controller.DropAsync(Target, CancellationToken.None);

        Assert.True(outcome.Succeeded);
        Assert.Equal([new ReparentRequest(Node, Target)], editor.Applied);
        Assert.False(controller.IsDragging);

        var edit = Assert.Single(undo.Edits);
        await edit.Undo(CancellationToken.None);
        Assert.Equal(2, editor.Applied.Count);
        Assert.Equal(Node, editor.Applied[1].Node);
        Assert.Null(editor.Applied[1].Target);
    }

    [Fact]
    public async Task ARefusedDropNeverReachesTheDocument()
    {
        var editor = new FakeHierarchyEditor
        {
            Decide = _ => ReparentDecision.Refuse("that move would create a cycle"),
        };
        var undo = new RecordingUndo();
        var controller = new HierarchyReparentController(editor, undo);

        controller.BeginDrag(Node);
        controller.Hover(Target);
        var decision = await controller.ResolveHoverAsync(CancellationToken.None);
        Assert.False(decision!.Allowed);
        Assert.Equal("that move would create a cycle", controller.LastRefusal);

        var outcome = await controller.DropAsync(Target, CancellationToken.None);

        Assert.False(outcome.Succeeded);
        Assert.Equal("that move would create a cycle", outcome.RefusalReason);
        Assert.Empty(editor.Applied);
        Assert.Empty(undo.Edits);
    }

    [Fact]
    public async Task TheServerOwnsCycleValidityNotTheClient()
    {
        // The target is a descendant of the node, which a client-side ancestor walk would
        // refuse. The server allows it, so it must be applied: the view may not decide.
        var descendant = new GraphNodeKey("Account", "a-1");
        var editor = new FakeHierarchyEditor { Decide = _ => ReparentDecision.Allow };
        var controller = new HierarchyReparentController(editor);

        controller.BeginDrag(Node);
        var outcome = await controller.DropAsync(descendant, CancellationToken.None);

        Assert.True(outcome.Succeeded);
        Assert.Equal(descendant, Assert.Single(editor.Applied).Target);
    }

    [Fact]
    public async Task CutAndPasteIsTheKeyboardAlternative()
    {
        var editor = new FakeHierarchyEditor();
        var undo = new RecordingUndo();
        var controller = new HierarchyReparentController(editor, undo);

        controller.Cut(Node);
        Assert.True(controller.IsCutPending);

        var outcome = await controller.PasteAsync(Target, CancellationToken.None);

        Assert.True(outcome.Succeeded);
        Assert.False(controller.IsCutPending);
        Assert.Single(editor.Applied);
        Assert.Single(undo.Edits);
    }

    [AvaloniaFact]
    public void ADragFromOneRowToAnotherReparentsThroughTheServer()
    {
        var editor = new FakeHierarchyEditor();
        var undo = new RecordingUndo();
        var tree = new SpotHierarchy
        {
            Source = HierarchySamples.AccountsSource(1_000, branching: 8),
            Editor = editor,
            Undo = undo,
            Clock = new FixedClock(),
        };
        var window = TestHost.Show(tree, 600, 400);
        TestHost.Pump();
        GraphFixtures.Press(tree, Avalonia.Input.Key.Right);
        TestHost.Pump();

        var rowHeight = tree.RowHeight;
        var firstY = tree.RowY(1) + (rowHeight / 2);
        var targetY = tree.RowY(3) + (rowHeight / 2);
        var dragged = tree.Model!.Rows[1].Key;
        var target = tree.Model.Rows[3].Key;

        window.MouseDown(new Avalonia.Point(100, firstY), Avalonia.Input.MouseButton.Left);
        window.MouseMove(new Avalonia.Point(120, firstY + 8), Avalonia.Input.RawInputModifiers.LeftMouseButton);
        window.MouseMove(new Avalonia.Point(120, targetY), Avalonia.Input.RawInputModifiers.LeftMouseButton);
        window.MouseUp(new Avalonia.Point(120, targetY), Avalonia.Input.MouseButton.Left);
        TestHost.Pump();
        TestHost.Pump();

        var applied = Assert.Single(editor.Applied);
        Assert.Equal(dragged, applied.Node);
        Assert.Equal(target, applied.Target);
        Assert.True(tree.Reparent!.IsDragging == false);
        Assert.Single(undo.Edits);
    }

    [AvaloniaFact]
    public void AReleaseOutsideEveryRowCancelsTheDragInsteadOfMovingToTheTopLevel()
    {
        var editor = new FakeHierarchyEditor();
        var undo = new RecordingUndo();
        var tree = new SpotHierarchy
        {
            Source = HierarchySamples.AccountsSource(1_000, branching: 8),
            Editor = editor,
            Undo = undo,
            Clock = new FixedClock(),
        };
        var window = TestHost.Show(tree, 600, 400);
        TestHost.Pump();
        GraphFixtures.Press(tree, Avalonia.Input.Key.Right);
        TestHost.Pump();

        var firstY = tree.RowY(1) + (tree.RowHeight / 2);
        window.MouseDown(new Avalonia.Point(100, firstY), Avalonia.Input.MouseButton.Left);
        window.MouseMove(new Avalonia.Point(120, firstY + 8), Avalonia.Input.RawInputModifiers.LeftMouseButton);
        window.MouseMove(new Avalonia.Point(120, 2), Avalonia.Input.RawInputModifiers.LeftMouseButton);
        window.MouseUp(new Avalonia.Point(120, 2), Avalonia.Input.MouseButton.Left);
        TestHost.Pump();
        TestHost.Pump();

        Assert.Empty(editor.Applied);
        Assert.Empty(undo.Edits);
        Assert.False(tree.Reparent!.IsDragging);
    }

    [AvaloniaFact]
    public void ControlXAndControlVPasteThroughTheControl()
    {
        var editor = new FakeHierarchyEditor();
        var undo = new RecordingUndo();
        var tree = GraphFixtures.MakeHierarchy(HierarchySamples.AccountsSource(1_000, branching: 8));
        tree.Editor = editor;
        tree.Undo = undo;

        GraphFixtures.Press(tree, Avalonia.Input.Key.Right);
        TestHost.Pump();
        GraphFixtures.Press(tree, Avalonia.Input.Key.Down);
        var moved = tree.Model!.Cursor!.Value;
        GraphFixtures.Press(tree, Avalonia.Input.Key.X, Avalonia.Input.KeyModifiers.Control);
        Assert.True(tree.Reparent!.IsCutPending);

        GraphFixtures.Press(tree, Avalonia.Input.Key.Down);
        GraphFixtures.Press(tree, Avalonia.Input.Key.V, Avalonia.Input.KeyModifiers.Control);
        TestHost.Pump();
        TestHost.Pump();

        var applied = Assert.Single(editor.Applied);
        Assert.Equal(moved, applied.Node);
        Assert.Single(undo.Edits);
        Assert.True(tree.Model.IsExpanded(GraphFixtures.RootKey), "a confirmed move must not collapse the tree");
    }

    [Fact]
    public async Task ADropOnItselfSendsNothing()
    {
        var editor = new FakeHierarchyEditor();
        var controller = new HierarchyReparentController(editor);
        controller.BeginDrag(Node);

        var outcome = await controller.DropAsync(Node, CancellationToken.None);

        Assert.False(outcome.Succeeded);
        Assert.Empty(editor.Checks);
        Assert.Empty(editor.Applied);
    }

    [Fact]
    public void EscapeCancelsADragWithoutApplying()
    {
        var editor = new FakeHierarchyEditor();
        var controller = new HierarchyReparentController(editor);
        controller.BeginDrag(Node);
        controller.Hover(Target);

        controller.CancelDrag();

        Assert.False(controller.IsDragging);
        Assert.Null(controller.HoverTarget);
        Assert.Empty(editor.Applied);
    }
}
