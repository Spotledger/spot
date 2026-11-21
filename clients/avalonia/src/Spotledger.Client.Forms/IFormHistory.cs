using Spotledger.Client.ChildTable;

namespace Spotledger.Client.Forms;

/// <summary>
/// The form's undo and redo history as the form and its view see it (WP-UI-17). The host owns the
/// history (the Workspace stack, behind an adapter) so <c>Forms</c> does not reference
/// <c>Workspace</c>. Nothing here sends anything to the server: undo and redo only edit the draft.
/// </summary>
public interface IFormHistory
{
    /// <summary>True when there is a step to undo.</summary>
    bool CanUndo { get; }

    /// <summary>True when there is a step to redo.</summary>
    bool CanRedo { get; }

    /// <summary>The name of the step Undo would reverse ("Edit Quantity"), or null.</summary>
    string? UndoLabel { get; }

    /// <summary>The name of the step Redo would replay, or null.</summary>
    string? RedoLabel { get; }

    /// <summary>Raised when the history, a label or the availability changed.</summary>
    event EventHandler? Changed;

    /// <summary>Raised when the history was cleared (save, reload, revert); the argument is the note to show.</summary>
    event EventHandler<string>? Cleared;

    /// <summary>Reverses the last step; the result says what happened (a refusal changes nothing).</summary>
    ChildTableResult Undo();

    /// <summary>Replays the last undone step.</summary>
    ChildTableResult Redo();

    /// <summary>Runs a change the person did not make by hand (a server form-event diff) so it is undone and redone with the edit that caused it.</summary>
    void RunDerived(Action change);

    /// <summary>The history seam of one child table: its Ctrl+Z and Ctrl+Y go to the form's history.</summary>
    IChildTableHistory ForTable(string table);

    /// <summary>Supplies the checks that decide whether a step may be undone or redone now.</summary>
    /// <param name="table">The check for a child table by field name (null: the table may change).</param>
    /// <param name="field">The check for a header field by name (null: the field may change).</param>
    void SetGate(Func<string, ChildTableResult?> table, Func<string, ChildTableResult?>? field);
}
