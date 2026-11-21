using Spotledger.Client.ChildTable;

namespace Spotledger.Client.Workspace;

/// <summary>
/// One child table's view of one form's history. It hands the table's Ctrl+Z, Ctrl+Y and Ctrl+Shift+Z
/// (and the table's public Undo and Redo) to <see cref="UndoStack.TryUndo"/> and
/// <see cref="UndoStack.TryRedo"/>, so a key pressed in the grid undoes the form's last step — a header
/// field edit, a row operation or a paste — and the stack, not the table, replays the row journal.
///
/// <para>It adds no checks of its own: the stack's gate decides (the host supplies it with
/// <see cref="UndoStack.SetGate"/>). Its only work is to place the cursor: when the replayed step
/// touched this table, the result names the row it landed on, and a step on another table (or a header
/// step) leaves this table's cursor alone.</para>
/// </summary>
public sealed class ChildTableHistoryAdapter : IChildTableHistory
{
    private readonly UndoStack _stack;
    private readonly string _tableName;

    /// <summary>Creates the adapter over one form's stack for one child table.</summary>
    /// <param name="stack">The form's history.</param>
    /// <param name="tableName">The table field's name on the document.</param>
    public ChildTableHistoryAdapter(UndoStack stack, string tableName)
    {
        ArgumentNullException.ThrowIfNull(stack);
        ArgumentException.ThrowIfNullOrWhiteSpace(tableName);
        _stack = stack;
        _tableName = tableName;
    }

    /// <inheritdoc />
    public bool CanUndo => _stack.CanUndo;

    /// <inheritdoc />
    public bool CanRedo => _stack.CanRedo;

    /// <inheritdoc />
    public ChildTableResult Undo() => Place(_stack.TryUndo());

    /// <inheritdoc />
    public ChildTableResult Redo() => Place(_stack.TryRedo());

    /// <summary>Keeps the cursor the stack named only when the row belongs to this table.</summary>
    private ChildTableResult Place(ChildTableResult result)
    {
        if (!result.Succeeded || result.Cursor is null)
        {
            return result;
        }

        return _stack.LastReplayTarget is { } target && string.Equals(target.Table, _tableName, StringComparison.Ordinal)
            ? result
            : ChildTableResult.Ok();
    }
}
