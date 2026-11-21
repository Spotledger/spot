namespace Spotledger.Client.Workspace.Split;

/// <summary>How a person answered the unsaved-changes guard.</summary>
public enum GuardChoice
{
    /// <summary>Save the dirty form, then select the row that was waiting.</summary>
    Save,

    /// <summary>Throw the changes away and select the row that was waiting.</summary>
    Discard,

    /// <summary>Keep the form and its changes; the selection goes back to the document on screen.</summary>
    Cancel,
}

/// <summary>
/// The unsaved-changes guard of the arrangement, as a state machine with no view in it. A selection
/// change never closes a dirty form silently: the first attempt is blocked, the target row is held as
/// <see cref="PendingId"/>, and only an answer to the guard (<see cref="Commit"/> after Save or
/// Discard) lets it through. "Cancel" clears the pending row and puts the list's selection back.
/// </summary>
public sealed class SplitSelectionGuard
{
    /// <summary>The row the person asked for while the form was dirty; null when nothing is pending.</summary>
    public string? PendingId { get; private set; }

    /// <summary>The document whose unsaved changes are holding the selection back.</summary>
    public string? HeldId { get; private set; }

    /// <summary>True while a selection change waits for an answer.</summary>
    public bool IsPending { get; private set; }

    /// <summary>The question the guard shows, in the person's words.</summary>
    public string Question => IsPending
        ? "This document has unsaved changes. Save them, discard them, or stay here?"
        : string.Empty;

    /// <summary>
    /// Decides whether the list may move to <paramref name="next"/>. True means "go ahead now"; false
    /// means the change is now pending and the guard must be shown.
    /// </summary>
    /// <param name="current">The document the form holds (null: nothing is open).</param>
    /// <param name="next">The row that was selected (null: the selection was cleared).</param>
    /// <param name="dirty">Whether the open form has unsaved changes.</param>
    public bool TryChange(string? current, string? next, bool dirty)
    {
        if (!dirty || current is null || string.Equals(current, next, StringComparison.Ordinal))
        {
            Reset();
            return true;
        }

        PendingId = next;
        HeldId = current;
        IsPending = true;
        return false;
    }

    /// <summary>The row that may now be shown; clears the pending state.</summary>
    public string? Commit()
    {
        var pending = PendingId;
        Reset();
        return pending;
    }

    /// <summary>"Stay here": forgets the pending row (the caller restores the list's selection).</summary>
    public void Cancel() => Reset();

    /// <summary>Clears everything (a save or a delete settled the form).</summary>
    public void Reset()
    {
        PendingId = null;
        HeldId = null;
        IsPending = false;
    }
}
