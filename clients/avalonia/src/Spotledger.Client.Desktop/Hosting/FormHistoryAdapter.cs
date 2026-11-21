using Spotledger.Client.ChildTable;
using Spotledger.Client.Forms;
using Spotledger.Client.Workspace;

namespace Spotledger.Client.Desktop.Hosting;

/// <summary>
/// The Workspace undo stack as the form's <see cref="IFormHistory"/>. Forms cannot reference
/// Workspace, so the host stands this adapter between them. It adds no rule: every undo and redo is
/// decided by the stack's gate.
/// </summary>
internal sealed class FormHistoryAdapter : IFormHistory, IDisposable
{
    private readonly UndoStack _stack;

    public FormHistoryAdapter(UndoStack stack)
    {
        _stack = stack ?? throw new ArgumentNullException(nameof(stack));
        _stack.Changed += (_, e) => Changed?.Invoke(this, e);
        _stack.Cleared += (_, note) => Cleared?.Invoke(this, note);
    }

    /// <summary>The stack behind the adapter (the Edit commands and their availability read it).</summary>
    public UndoStack Stack => _stack;

    public bool CanUndo => _stack.CanUndo;

    public bool CanRedo => _stack.CanRedo;

    public string? UndoLabel => _stack.UndoLabel;

    public string? RedoLabel => _stack.RedoLabel;

    public event EventHandler? Changed;

    public event EventHandler<string>? Cleared;

    public ChildTableResult Undo() => _stack.TryUndo();

    public ChildTableResult Redo() => _stack.TryRedo();

    public void RunDerived(Action change) => _stack.RunDerived(change);

    public IChildTableHistory ForTable(string table) => new ChildTableHistoryAdapter(_stack, table);

    public void SetGate(Func<string, ChildTableResult?> table, Func<string, ChildTableResult?>? field)
        => _stack.SetGate(table, field);

    public void Dispose() => _stack.Dispose();
}
