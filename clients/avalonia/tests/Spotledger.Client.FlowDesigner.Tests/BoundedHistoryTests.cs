using System;
using System.Threading;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The undo and redo history is bounded: past the limit the oldest step is dropped and everything
/// kept behaves exactly as before. The tests assert on counts and on the exact undo order, not on bytes.
public sealed class BoundedHistoryTests
{
    private static string Title(int edit) => "t" + edit;

    [Fact]
    public void TheDefaultBoundIs500AndAZeroBoundIsRefused()
    {
        Assert.Equal(500, new FlowEditor(FlowFixtures.Document()).MaxHistory);
        Assert.Throws<ArgumentOutOfRangeException>(() => new FlowEditor(FlowFixtures.Document(), 0));
    }

    [Fact]
    public void ThousandEditsWithABoundOfFiftyKeepFiftyStepsUndoneInExactReverseOrder()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document, 50);

        for (int i = 1; i <= 1000; i++)
        {
            Assert.True(editor.Execute(new SetTitleCommand(Title(i))));
        }

        Assert.Equal(50, editor.UndoDepth);
        Assert.Equal(0, editor.RedoDepth);
        Assert.Equal(Title(1000), document.Title);

        for (int i = 1000; i > 950; i--)
        {
            Assert.True(editor.Undo());
            Assert.Equal(Title(i - 1), document.Title);
        }

        Assert.False(editor.Undo());
        Assert.False(editor.CanUndo);
        Assert.Equal(50, editor.RedoDepth);
        Assert.Equal(Title(950), document.Title);
    }

    [Fact]
    public void RedoReappliesTheKeptStepsInOrder()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document, 50);
        for (int i = 1; i <= 120; i++)
        {
            editor.Execute(new SetTitleCommand(Title(i)));
        }

        while (editor.Undo())
        {
        }

        Assert.Equal(Title(70), document.Title);
        for (int i = 71; i <= 120; i++)
        {
            Assert.True(editor.Redo());
            Assert.Equal(Title(i), document.Title);
        }

        Assert.False(editor.Redo());
    }

    [Fact]
    public void ABoundOfOneKeepsOnlyTheLastEdit()
    {
        FlowDocument document = FlowFixtures.Document();
        FlowEditor editor = new(document, 1);

        editor.Execute(new SetTitleCommand("one"));
        editor.Execute(new SetTitleCommand("two"));

        Assert.Equal(1, editor.UndoDepth);
        Assert.True(editor.Undo());
        Assert.Equal("one", document.Title);
        Assert.False(editor.Undo());
        Assert.Equal(1, editor.RedoDepth);
        Assert.True(editor.Redo());
        Assert.Equal("two", document.Title);
    }

    [Fact]
    public void ANewEditClearsRedoAsBefore()
    {
        FlowEditor editor = new(FlowFixtures.Document(), 3);
        editor.Execute(new SetTitleCommand("a"));
        editor.Execute(new SetTitleCommand("b"));
        editor.Undo();
        Assert.Equal(1, editor.RedoDepth);

        editor.Execute(new SetTitleCommand("c"));

        Assert.Equal(0, editor.RedoDepth);
        Assert.Equal(2, editor.UndoDepth);
    }

    [Fact]
    public void LabelsFollowTheBoundedStacks()
    {
        FlowEditor editor = new(FlowFixtures.Document(), 2);
        Assert.Null(editor.UndoLabel);
        editor.Execute(new SetTitleCommand("a"));
        Assert.Equal("Change the title", editor.UndoLabel);
        editor.Undo();
        Assert.Null(editor.UndoLabel);
        Assert.Equal("Change the title", editor.RedoLabel);
    }

    [Fact]
    public void ADroppedCommandLeavesTheWorkspaceEntriesValid()
    {
        FlowDocument document = FlowFixtures.Document();
        RecordingWorkspaceUndo workspace = new();
        FlowEditor editor = new(document, 5) { WorkspaceUndo = workspace };

        for (int i = 1; i <= 20; i++)
        {
            editor.Execute(new SetTitleCommand(Title(i)));
        }

        Assert.Equal(20, workspace.Entries.Count);
        Assert.Equal(5, editor.UndoDepth);

        // The newest entry undoes the last edit; the oldest (a dropped command) still runs.
        TestHost.Await(workspace.Entries[19].Undo(CancellationToken.None));
        Assert.Equal(Title(19), document.Title);
        TestHost.Await(workspace.Entries[0].Undo(CancellationToken.None));
        Assert.Equal(FlowFixtures.Document().Title, document.Title);
    }
}
