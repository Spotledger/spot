namespace Spotledger.Client.ChildTable;

/// <summary>
/// The form's undo history as a child table sees it: the seam that lets a table hand Ctrl+Z, Ctrl+Y
/// and Ctrl+Shift+Z — and its own <see cref="SpotChildTable.Undo"/> and <see cref="SpotChildTable.Redo"/> —
/// to the one history of the document, instead of replaying its own row journal behind that history's
/// back (which leaves the two out of step).
///
/// <para>Every answer is a <see cref="ChildTableResult"/>, as for every other table operation, so a
/// refusal or "nothing to undo" reaches the host through the same
/// <see cref="SpotChildTable.OperationCompleted"/> path as a row edit does. The history does its own
/// gating: the form's edit gate decides whether anything may change.</para>
/// </summary>
public interface IChildTableHistory
{
    /// <summary>True when there is a step to undo.</summary>
    bool CanUndo { get; }

    /// <summary>True when there is a step to redo.</summary>
    bool CanRedo { get; }

    /// <summary>Undoes the form's last step (which may be a header field edit, a row operation or a paste).</summary>
    ChildTableResult Undo();

    /// <summary>Replays the form's last undone step.</summary>
    ChildTableResult Redo();
}
