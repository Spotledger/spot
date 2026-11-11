using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms;

public sealed partial class DocumentFormViewModel
{
    /// <summary>
    /// Applies a change that the server's form-event answer made to the draft (a derived total, a
    /// fetched field) inside the history's derived scope, so one Undo reverses the edit that caused it
    /// together with what the server derived from it. Without a history the change is applied as is.
    /// </summary>
    public void ApplyServerDiff(Action<DocumentDraft> change)
    {
        ArgumentNullException.ThrowIfNull(change);
        var draft = _draft ?? throw new InvalidOperationException("The form is not loaded.");
        if (_history is { } history)
        {
            history.RunDerived(() => change(draft));
        }
        else
        {
            change(draft);
        }
    }

    /// <summary>
    /// The check a header field must pass before the history changes it (undo, redo): the write right,
    /// then the field's own state. Masked, hidden and non-editable (permission) fields refuse. A
    /// field that is read-only by its metadata (or, while shown, by a rule) does <b>not</b> refuse: such a field (a total,
    /// a fetched value) can only have changed as a derived consequence of an edit that was allowed,
    /// and refusing it would make that edit impossible to undo.
    /// </summary>
    public ChildTableResult? CheckFieldForHistory(string name)
    {
        var draft = _draft;
        if (draft is null || !draft.Fields.TryGetValue(name, out var state))
        {
            return ChildTableResult.Refused(ChildTableRefusal.OutOfRange, $"The field {name} is no longer on this document.");
        }

        if (IsNew ? !_rights.Create : !_rights.Write)
        {
            return ChildTableResult.Refused(ChildTableRefusal.NoWriteRight);
        }

        if (state.IsMasked)
        {
            return ChildTableResult.Refused(ChildTableRefusal.CellMasked, $"The field {name} is masked for you, so the change is refused.");
        }

        if (!state.IsVisible)
        {
            return ChildTableResult.Refused(ChildTableRefusal.CellHidden, $"The field {name} is hidden, so the change is refused.");
        }

        // IsEditable folds the metadata's read-only flag in with the permission summary; only the
        // metadata flag (a derived field) is tolerated, so a field the permission summary took away,
        // or one of an unknown kind, still refuses.
        return state.IsEditable || draft.Model.Field(name)?.ReadOnly == true
            ? null
            : ChildTableResult.Refused(ChildTableRefusal.CellReadOnly, $"You cannot change the field {name}, so the change is refused.");
    }

    private ChildTableResult? CheckTableForHistory(string table)
    {
        if (_draft is null)
        {
            return ChildTableResult.Refused(ChildTableRefusal.OutOfRange);
        }

        // Fail closed: until the view has handed over the table's own check, no table step runs.
        return TableGate is { } gate
            ? gate(table)
            : ChildTableResult.Refused(ChildTableRefusal.NoWriteRight, $"The table {table} is not shown, so the change is refused.");
    }

    private void AttachHistory(DocumentDraft draft)
    {
        DetachHistory();
        HistoryNote = null;
        if (HistoryFactory?.Invoke(draft) is not { } history)
        {
            return;
        }

        _history = history;
        history.SetGate(CheckTableForHistory, CheckFieldForHistory);
        history.Cleared += OnHistoryCleared;
        OnPropertyChanged(nameof(History));
    }

    private void DetachHistory()
    {
        if (_history is null)
        {
            return;
        }

        _history.Cleared -= OnHistoryCleared;
        (_history as IDisposable)?.Dispose();
        _history = null;
        OnPropertyChanged(nameof(History));
    }

    private void OnHistoryCleared(object? sender, string note) => HistoryNote = note;

    private async Task NotifyOpenedAsync()
    {
        if (Opened is not { } opened)
        {
            return;
        }

        try
        {
            await opened(this);
        }
        catch (Exception e) when (e is not OperationCanceledException)
        {
            // Draft recovery must never stop a form from opening; say so instead of failing quietly.
            AddMessage("warning", "draft.unavailable", "Unsaved-change recovery is not available: " + e.GetType().Name + ".");
        }
    }
}
