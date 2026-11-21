using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The command stack: every edit is one command with an exact inverse, so undo restores the text
/// form byte for byte and redo reapplies it. The tests compare the whole text form, not just the
/// changed node, because a command that quietly reorders a definition would otherwise pass.
public sealed class CommandUndoTests
{
    [Fact]

    public void AddingANodeIsUndoneBackToTheSameTextForm()
    {
        FlowDocument document = FlowFixtures.Document();
        string before = document.ToText();
        FlowEditor editor = new(document);

        FlowNode node = FlowFixtures.Catalogue().Schema("decision")!.NewNode("approve_delivery");
        node.Position = FlowLayoutEngine.Snap(new FlowPoint(640, 480));
        Assert.True(editor.Execute(new AddNodeCommand(node)));

        Assert.NotNull(document.Node("approve_delivery"));
        Assert.NotEqual(before, document.ToText());

        Assert.True(editor.Undo());
        Assert.Equal(before, document.ToText(), StringComparer.Ordinal);

        Assert.True(editor.Redo());
        Assert.NotNull(document.Node("approve_delivery"));
    }

    [Fact]

    public void DeletingANodeTakesItsEdgesAndUndoRestoresBothInOrder()
    {
        FlowDocument document = FlowFixtures.Document();
        string before = document.ToText();
        int edgeCount = document.Edges.Count;
        FlowEditor editor = new(document);

        Assert.True(editor.Execute(new DeleteSelectionCommand(["create_delivery"], [])));

        Assert.Null(document.Node("create_delivery"));
        Assert.Equal(edgeCount - 2, document.Edges.Count);

        Assert.True(editor.Undo());
        Assert.Equal(before, document.ToText(), StringComparer.Ordinal);
        Assert.Equal(edgeCount, document.Edges.Count);
    }

    [Fact]

    public void DeletingAnEdgeLeavesTheNodesAlone()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document);
        int before = document.Nodes.Count;

        Assert.True(editor.Execute(new DeleteSelectionCommand([], [new FlowEdgeKey("approve", "notify")])));

        Assert.Equal(before, document.Nodes.Count);
        Assert.DoesNotContain(document.Edges, edge => edge.Key == new FlowEdgeKey("approve", "notify"));
        Assert.True(editor.Undo());
        Assert.Contains(document.Edges, edge => edge.Key == new FlowEdgeKey("approve", "notify"));
    }

    [Fact]

    public void MovingIsSnappedAndUndoneExactly()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowPoint start = document.Node("start")!.Position;
        FlowEditor editor = new(document);

        MoveNodesCommand move = new(document, ["start"], FlowLayoutEngine.GridSize, FlowLayoutEngine.GridSize * 2);
        Assert.True(move.HasMovement);
        Assert.True(editor.Execute(move));

        Assert.Equal(start.Offset(FlowLayoutEngine.GridSize, FlowLayoutEngine.GridSize * 2), document.Node("start")!.Position);

        Assert.True(editor.Undo());
        Assert.Equal(start, document.Node("start")!.Position);
    }

    [Fact]

    public void SettingAConfigValueIsOneUndoableStepAndRemovingItRestoresTheAbsence()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document);

        Assert.True(editor.Execute(new SetConfigCommand("create_delivery", "doctype", FlowValue.Text("Pick List"))));
        Assert.Equal(FlowValue.Text("Pick List"), document.Node("create_delivery")!.ConfigValue("doctype"));

        Assert.True(editor.Undo());
        Assert.Equal(FlowValue.Text("Delivery Note"), document.Node("create_delivery")!.ConfigValue("doctype"));

        Assert.True(editor.Execute(new SetConfigCommand("create_delivery", "mapping", FlowValue.Text("x"))));
        Assert.True(editor.Undo());
        Assert.Null(document.Node("create_delivery")!.ConfigValue("mapping"));
    }

    [Fact]

    public void TheEdgeAndNodeSettingsCommandsRoundTrip()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document);
        string before = document.ToText();

        Assert.True(editor.Execute(new SetEdgeFieldCommand(new FlowEdgeKey("approve", "notify"), "condition", "doc.total > 500")));
        FlowNodeSettings settings = new(FlowJoinKind.Quorum, 2, true, "hold", 45, 3, 10);
        Assert.True(editor.Execute(new SetNodeSettingsCommand("approve", settings)));
        Assert.True(editor.Execute(new SetTitleCommand("Order to cash, revised")));

        Assert.Contains("condition = \"doc.total > 500\"", document.ToText(), StringComparison.Ordinal);
        Assert.Contains("join = \"quorum\"\njoin_count = 2", document.ToText(), StringComparison.Ordinal);

        Assert.True(editor.Undo());
        Assert.True(editor.Undo());
        Assert.True(editor.Undo());
        Assert.Equal(before, document.ToText(), StringComparer.Ordinal);
    }

    [Fact]

    public void ARedoStackIsClearedByANewEdit()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document);
        Assert.True(editor.Execute(new SetTitleCommand("one")));
        Assert.True(editor.Undo());
        Assert.True(editor.CanRedo);

        Assert.True(editor.Execute(new SetTitleCommand("two")));

        Assert.False(editor.CanRedo);
        Assert.Equal("two", document.Title);
    }

    [Fact]

    public void AReadOnlyEditorRefusesEveryEdit()
    {
        FlowDocument document = FlowFixtures.Document();
        string before = document.ToText();
        FlowEditor editor = new(document) { CanEdit = false };

        Assert.False(editor.Execute(new SetTitleCommand("nope")));
        Assert.False(editor.Undo());
        Assert.False(editor.Redo());
        Assert.Equal(before, document.ToText(), StringComparer.Ordinal);
    }

    [Fact]

    public void TheWorkspaceUndoSeamReceivesTheInverseOfEveryEdit()
    {
        FlowDocument document = FlowFixtures.Document();
        RecordingWorkspaceUndo workspace = new();
        FlowEditor editor = new(document) { WorkspaceUndo = workspace };
        string before = document.ToText();

        Assert.True(editor.Execute(new SetTitleCommand("changed")));
        FlowUndoEntry entry = Assert.Single(workspace.Entries);

        Assert.Equal("Change the title", entry.Label);
        TestHost.Await(entry.Undo(CancellationToken.None));
        Assert.Equal(before, document.ToText(), StringComparer.Ordinal);
    }

    [Avalonia.Headless.XUnit.AvaloniaFact]

    public void PastingRenamesACollisionAndUndoRemovesOnlyTheCopies()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document);
        FlowCanvas canvas = FlowFixtures.Canvas(document);
        canvas.Editor = editor;
        canvas.SelectOnly(["start", "create_delivery"]);
        int nodes = document.Nodes.Count;

        Assert.True(canvas.CopySelection());
        Assert.True(canvas.PasteClipboard());

        Assert.Equal(nodes + 2, document.Nodes.Count);
        Assert.NotNull(document.Node("start_2"));
        Assert.NotNull(document.Node("create_delivery_2"));
        Assert.Contains(document.Edges, edge => edge.Key == new FlowEdgeKey("start_2", "create_delivery_2"));

        Assert.True(canvas.Undo());
        Assert.Equal(nodes, document.Nodes.Count);
        Assert.Null(document.Node("start_2"));
    }
}

/// Records the entries a host's undo stack would receive.
public sealed class RecordingWorkspaceUndo : IFlowWorkspaceUndo
{
    /// The recorded entries, oldest first.
    public List<FlowUndoEntry> Entries { get; } = [];

    public void Record(FlowUndoEntry entry) => Entries.Add(entry);
}
