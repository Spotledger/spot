namespace Spotledger.Client.Graph;

/// <summary>
/// The workspace navigation seam (WP-UI-17): opening a document from a graph view. The
/// shell supplies the implementation; these views never construct a host or a URL. A host
/// adapter binds the workspace's own navigation service here, so the graph project stays
/// free of Shell types.
/// </summary>
public interface IWorkspaceNavigation
{
    /// <summary>Opens the record in the workspace.</summary>
    /// <param name="key">The record to open.</param>
    /// <param name="cancellationToken">Cancels the request when the user navigates away.</param>
    Task OpenDocumentAsync(GraphNodeKey key, CancellationToken cancellationToken);
}

/// <summary>
/// The workspace undo seam (WP-UI-17): a graph edit registers its inverse here so it joins
/// the same undo stack as every other workspace edit. The host owns the stack; this project
/// only hands it an entry.
/// </summary>
public interface IWorkspaceUndo
{
    /// <summary>Registers one undoable graph edit.</summary>
    /// <param name="edit">The label and the inverse operation.</param>
    void Record(UndoableEdit edit);
}

/// <summary>One undoable edit handed to the workspace undo stack.</summary>
/// <param name="Label">The user-facing label ("Move Accounts to …").</param>
/// <param name="Undo">The inverse operation, run when the user undoes the edit.</param>
public sealed record UndoableEdit(string Label, Func<CancellationToken, Task> Undo)
{
    /// <summary>The graph edit that re-parented a node, as the undo stack records it.</summary>
    public static UndoableEdit Reparent(ReparentChange change, IHierarchyEditor editor)
    {
        ArgumentNullException.ThrowIfNull(editor);
        ArgumentNullException.ThrowIfNull(change);
        return new UndoableEdit(
            string.Concat("Move ", change.Node.Entity, " back"),
            async cancellationToken =>
            {
                // The inverse is a normal document update: the server re-checks it, so an
                // undo can be refused exactly like the original move.
                await editor.ApplyAsync(new ReparentRequest(change.Node, change.PreviousParent), cancellationToken)
                    .ConfigureAwait(true);
            });
    }
}

/// <summary>The time source the views read, injected so no logic calls the wall clock and
/// the type-ahead window is testable with an explicit timestamp.</summary>
public interface IGraphClock
{
    /// <summary>The current instant.</summary>
    DateTimeOffset Now { get; }
}

/// <summary>The production clock: the wall clock, read only by the control's input path.</summary>
public sealed class SystemGraphClock : IGraphClock
{
    /// <summary>The shared instance.</summary>
    public static SystemGraphClock Instance { get; } = new();

    /// <inheritdoc />
    public DateTimeOffset Now => DateTimeOffset.UtcNow;
}
